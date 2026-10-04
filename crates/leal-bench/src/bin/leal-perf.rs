//! Measures the app's DESIGN §1 budgets on this Mac and prints the table in
//! `docs/perf.md` (task 1.10). `just perf` builds the apps and runs it.
//!
//! ```text
//! leal-perf --app Leal.app --bench-app Leal.app --file reference.csv
//!           [--big-file big.csv] [--runs N] [--speed fast|moderate]
//!           [--no-scroll | --only-scroll | --only-edit] [--compare-drawing]
//!           [--out DIR]
//! ```
//!
//! Every launch is the way the app ships: `open`, so LaunchServices starts
//! it sandboxed, in front (docs/tasks/1.6.md, "Scroll performance": shell
//! launches run on other cores and aren't comparable). It never sends
//! input to the system, and quits only the processes it started, by PID
//! (CLAUDE.md).
//!
//! - **Launch** and **idle memory**: `--app` with no document. The
//!   "Launched" signpost gives the time from the process starting; after
//!   `--settle` seconds `heap -s` gives the footprint (which the idle
//!   budget is judged on) and heap, and `ps` the resident size.
//! - **Open**, **index** and **heap**: `--app` with `--file`. The app's
//!   "Open to first rows" signpost and the core's "First paint" and "Index"
//!   give the times; `heap -s` the memory once the review has finished and
//!   the app has settled. This cold open is judged as part of launch; the
//!   open budget is judged on opens in a running app (`--bench-app`,
//!   `-LealReopen`).
//! - **Cell edit to screen** (task 2.5.1): `--bench-app` with the
//!   reference file edits cells through the in-cell editor by itself
//!   (`-LealBenchEdit`), each once the last is on screen; its "Cell edit to
//!   screen" signposts give the times. `--only-edit` runs only these.
//! - **Scrolling**: `--bench-app` (a `LEAL_BENCH` build) scrolling itself
//!   (`ScrollBench`): after the load; from the first rows, with a search
//!   running (background work pausing as rule 3 says); and on `--big-file`
//!   from the first rows with nothing pausing (the stress case). While each
//!   runs, WindowServer's CPU time is read with `ps` every 250 ms, for the
//!   render server's share of each frame (task 2.0b). `--only-scroll` runs
//!   only these. `--compare-drawing` runs each twice, alternating, with the
//!   grid drawn into strips (as it ships) and by AppKit (`-LealStrips NO`,
//!   as before task 2.0b), and reports both; the verdicts are the strips'.
//!
//! The verdicts follow DESIGN §1's notes on what each budget means.
//!
//! Writes every run's numbers to `--out` (default `target/perf`) as JSON.

use std::collections::HashMap;
use std::io::{BufRead as _, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use leal_bench::perf::{
    HeapReport, PerfRun, Row, SCENARIOS, ScrollRun, Signpost, Spread, Timeline, collect,
    cpu_between, launched_after_ms, parse_ps_time, table,
};
use serde_json::{Value, json};

const USAGE: &str = "usage: leal-perf --report FILE.json | --app APP --bench-app APP --file CSV [--big-file CSV] [--runs N] [--speed fast|moderate] [--find TEXT] [--settle SECONDS] [--no-scroll | --only-scroll | --only-edit] [--compare-drawing] [--out DIR]";

/// The app's sandbox container, where the scroll benchmark writes.
const CONTAINER_TMP: &str = "Library/Containers/io.github.robhaswell.leal/Data/tmp";

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("error: {message}");
            ExitCode::FAILURE
        }
    }
}

struct Options {
    app: PathBuf,
    bench_app: PathBuf,
    file: PathBuf,
    big_file: Option<PathBuf>,
    runs: usize,
    speed: String,
    find: String,
    settle: Duration,
    scroll: bool,
    /// Only the scroll runs: no launches, opens, reopens or edits.
    only_scroll: bool,
    /// Only the edit runs (task 2.5.1).
    only_edit: bool,
    /// Each scroll run twice, with strips and with AppKit's drawing.
    compare_drawing: bool,
    out: PathBuf,
}

fn options() -> Result<Options, String> {
    let mut app = None;
    let mut bench_app = None;
    let mut file = None;
    let mut big_file = None;
    let mut runs = 3;
    let mut speed = "fast".to_owned();
    let mut find = "SKU-".to_owned();
    let mut settle = Duration::from_secs(5);
    let mut scroll = true;
    let mut only_scroll = false;
    let mut only_edit = false;
    let mut compare_drawing = false;
    let mut out = PathBuf::from("target/perf");
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut value = || args.next().ok_or(format!("{arg} needs a value"));
        match arg.as_str() {
            "--app" => app = Some(PathBuf::from(value()?)),
            "--bench-app" => bench_app = Some(PathBuf::from(value()?)),
            "--file" => file = Some(PathBuf::from(value()?)),
            "--big-file" => big_file = Some(PathBuf::from(value()?)),
            "--runs" => runs = value()?.parse().map_err(|e| format!("--runs: {e}"))?,
            "--speed" => speed = value()?,
            "--find" => find = value()?,
            "--settle" => {
                settle = Duration::from_secs_f64(
                    value()?.parse().map_err(|e| format!("--settle: {e}"))?,
                );
            }
            "--no-scroll" => scroll = false,
            "--only-scroll" => only_scroll = true,
            "--only-edit" => only_edit = true,
            "--compare-drawing" => compare_drawing = true,
            "--out" => out = PathBuf::from(value()?),
            "-h" | "--help" => return Err(USAGE.to_owned()),
            _ => return Err(format!("unexpected argument `{arg}`\n{USAGE}")),
        }
    }
    if only_scroll && !scroll {
        return Err(format!(
            "--only-scroll and --no-scroll leave nothing to run\n{USAGE}"
        ));
    }
    if only_edit && only_scroll {
        return Err(format!(
            "--only-edit and --only-scroll leave nothing to run\n{USAGE}"
        ));
    }
    let absolute =
        |p: PathBuf| std::path::absolute(&p).map_err(|e| format!("{}: {e}", p.display()));
    Ok(Options {
        app: absolute(app.ok_or(USAGE)?)?,
        bench_app: absolute(bench_app.ok_or(USAGE)?)?,
        file: absolute(file.ok_or(USAGE)?)?,
        big_file: big_file.map(absolute).transpose()?,
        runs: runs.max(1),
        speed,
        find,
        settle,
        scroll: scroll && !only_edit,
        only_scroll,
        only_edit,
        compare_drawing,
        out,
    })
}

fn run() -> Result<(), String> {
    // `--report FILE`: print the table again from a run's JSON.
    let args: Vec<String> = std::env::args().collect();
    if let [_, flag, file] = args.as_slice()
        && flag == "--report"
    {
        let text = std::fs::read_to_string(file).map_err(|e| format!("{file}: {e}"))?;
        let raw: Value = serde_json::from_str(&text).map_err(|e| format!("{file}: {e}"))?;
        println!("{}", render(&raw));
        return Ok(());
    }
    let options = options()?;
    for path in [&options.app, &options.bench_app, &options.file] {
        if !path.exists() {
            return Err(format!("{} doesn't exist", path.display()));
        }
    }
    let environment = environment();
    eprintln!(
        "leal-perf: {}",
        environment["summary"].as_str().unwrap_or("")
    );
    let mut launches = Vec::new();
    let mut opens = Vec::new();
    let mut reopens = Vec::new();
    let mut edits = Vec::new();
    if options.only_edit {
        let log = LogStream::start()?;
        for run in 1..=options.runs {
            eprintln!("leal-perf: edit {run} of {}", options.runs);
            edits.extend(edit_run(&options, &log)?);
        }
    } else if !options.only_scroll {
        let log = LogStream::start()?;
        // Launch and idle memory: no document.
        for run in 1..=options.runs {
            eprintln!("leal-perf: launch {run} of {}", options.runs);
            launches.push(launch_run(&options, &log)?);
        }
        // Open, index and the heap: the reference file.
        for run in 1..=options.runs {
            eprintln!("leal-perf: open {run} of {}", options.runs);
            opens.push(open_run(&options, &log)?);
        }
        // Opens in a running app: the bench build closes the file and
        // opens it again, five times.
        for run in 1..=options.runs {
            eprintln!("leal-perf: reopen {run} of {}", options.runs);
            reopens.extend(reopen_run(&options, &log)?);
        }
        // Cell edits in a running app: the bench build edits cells itself.
        for run in 1..=options.runs {
            eprintln!("leal-perf: edit {run} of {}", options.runs);
            edits.extend(edit_run(&options, &log)?);
        }
    }

    // Scrolling, in the bench build: with strips, and with
    // `--compare-drawing` with AppKit's drawing too, alternating which
    // goes first.
    let mut scrolls: HashMap<&str, Vec<Value>> = HashMap::new();
    let mut scrolls_appkit: HashMap<&str, Vec<Value>> = HashMap::new();
    if options.scroll {
        let mut scenarios: Vec<(&str, &Path, Vec<String>)> = vec![
            ("afterLoad", &options.file, vec![]),
            (
                "duringLoadWithFind",
                &options.file,
                vec![
                    "-LealBenchDuringLoad".into(),
                    "YES".into(),
                    "-LealBenchFind".into(),
                    options.find.clone(),
                ],
            ),
        ];
        if let Some(big) = &options.big_file {
            scenarios.push((
                "bigFileNoPause",
                big,
                vec![
                    "-LealBenchDuringLoad".into(),
                    "YES".into(),
                    "-LealBenchFind".into(),
                    options.find.clone(),
                    "-LealBenchNoPause".into(),
                    "YES".into(),
                ],
            ));
        }
        for (name, file, extra) in &scenarios {
            for run in 1..=options.runs {
                let mut drawings = vec![false];
                if options.compare_drawing {
                    drawings = if run % 2 == 1 {
                        vec![false, true]
                    } else {
                        vec![true, false]
                    };
                }
                for appkit in drawings {
                    let label = if appkit { " (AppKit's drawing)" } else { "" };
                    eprintln!("leal-perf: scroll {name}{label}, {run} of {}", options.runs);
                    let mut args = extra.clone();
                    if appkit {
                        args.extend(["-LealStrips".into(), "NO".into()]);
                    }
                    // A run that fails (the display slept, the app hung) is
                    // reported and left out; the others still count.
                    let json = match scroll_run(&options, file, &args) {
                        Ok(json) => json,
                        Err(message) => {
                            eprintln!(
                                "warning: scroll {name}{label} run {run} left out: {message}"
                            );
                            continue;
                        }
                    };
                    let parsed = ScrollRun::parse(&json).ok_or(format!(
                        "the scroll benchmark's JSON has no results: {json}"
                    ))?;
                    if parsed.stalls > 0 {
                        eprintln!(
                            "warning: scroll {name}{label} run {run}: the display link stopped for more than half a second; the run is kept but flagged"
                        );
                    }
                    let target = if appkit {
                        &mut scrolls_appkit
                    } else {
                        &mut scrolls
                    };
                    target.entry(name).or_default().push(json);
                }
            }
        }
    }

    let raw = json!({
        "environment": environment,
        "launches": launches,
        "opens": opens,
        "reopensMs": reopens,
        "editsMs": edits,
        "scrolls": scrolls,
        "scrollsAppKit": scrolls_appkit,
    });
    let report = render(&raw);
    println!("{report}");

    std::fs::create_dir_all(&options.out).map_err(|e| format!("{}: {e}", options.out.display()))?;
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let path = options.out.join(format!("perf-{stamp}.json"));
    std::fs::write(
        &path,
        serde_json::to_string_pretty(&raw).map_err(|e| e.to_string())?,
    )
    .map_err(|e| format!("{}: {e}", path.display()))?;
    std::fs::write(options.out.join("perf-latest.md"), &report)
        .map_err(|e| format!("writing the report: {e}"))?;
    eprintln!(
        "leal-perf: wrote {} and {}",
        path.display(),
        options.out.join("perf-latest.md").display()
    );
    Ok(())
}

// MARK: The table

/// The report for a run's numbers, as `leal-perf` saves them. The rows
/// and their verdicts are `PerfRun::rows`, in `leal_bench::perf`.
fn render(raw: &Value) -> String {
    let run = PerfRun::parse(raw);
    format_report(&raw["environment"], &run.rows(), &run)
}

fn format_report(environment: &Value, rows: &[Row], run: &PerfRun) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let _ = writeln!(out, "## leal-perf\n");
    if let Some(lines) = environment["lines"].as_array() {
        for line in lines {
            let _ = writeln!(out, "- {}", line.as_str().unwrap_or(""));
        }
    }
    let _ = writeln!(out, "\n{}", table(rows));
    let describe = |maps: &[HashMap<String, f64>], key: &str, unit: &str| {
        Spread::of(&collect(maps, key)).map_or("—".to_owned(), |s| s.describe(unit, 1))
    };
    let _ = writeln!(out, "Details:\n");
    let _ = writeln!(
        out,
        "- `open` to \"Launched\": {}",
        describe(&run.launches, "openCommandToLaunchedMs", "ms")
    );
    let _ = writeln!(
        out,
        "- Idle heap: {}",
        describe(&run.launches, "heapMB", "MB")
    );
    let _ = writeln!(
        out,
        "- With the file: the cold open (`read(from:)` to first rows, the app launched with it) {}; the core's first paint {}; footprint after opening {}",
        describe(&run.opens, "openToFirstRowsMs", "ms"),
        describe(&run.opens, "firstPaintMs", "ms"),
        describe(&run.opens, "footprintMB", "MB")
    );
    let figure =
        |v: Option<f64>, digits: usize| v.map_or("—".to_owned(), |v| format!("{v:.digits$}"));
    for (runs, kind) in [
        (&run.scrolls, ""),
        (&run.scrolls_appkit, ", AppKit's drawing"),
    ] {
        for name in SCENARIOS {
            for (i, r) in runs
                .get(name)
                .map_or(&[][..], Vec::as_slice)
                .iter()
                .enumerate()
            {
                let _ = writeln!(
                    out,
                    "- Scroll `{name}`{kind} run {}: {}, frame p99 {:.1} ms, main-thread CPU p50/p99 {}, {} frames busy over 8.3 ms, {:.1} M instructions a frame at {:.2} GHz; other threads {} ms and WindowServer {} ms a frame; while indexing {}; while searching {} ({} searches); heap peak {:.1} MB, footprint peak {} MB; drawn by {}; screen {} Hz{}",
                    i + 1,
                    ScrollRun::describe_late(r.late, r.frames),
                    r.p99_ms,
                    r.describe_cpu(),
                    r.busy_over_120hz,
                    r.instructions_mean,
                    r.ghz,
                    figure(r.other_threads_ms, 2),
                    figure(r.window_server_ms, 2),
                    ScrollRun::describe_late(r.while_indexing.0, r.while_indexing.1),
                    ScrollRun::describe_late(r.while_finding.0, r.while_finding.1),
                    r.find_runs,
                    r.heap_peak_mb,
                    figure(r.footprint_peak_mb, 0),
                    r.drawing.as_deref().unwrap_or("—"),
                    r.screen_fps,
                    match (r.stalls > 0, r.visible) {
                        (true, _) => " (flagged: the display link stopped)",
                        (false, false) => " (window not visible at the end: the screen was locked)",
                        _ => "",
                    }
                );
            }
        }
    }
    if !run.scrolls_appkit.is_empty() {
        let _ = writeln!(
            out,
            "\nDrawing compared (`--compare-drawing`, runs alternated; task 2.0b): strips against AppKit's drawing\n"
        );
        let _ = writeln!(
            out,
            "| Scroll run | Main-thread CPU p50 / p99 | Instructions a frame | Other threads | WindowServer | Late frames | Footprint peak |\n|---|---|---|---|---|---|---|"
        );
        let spread = |runs: &[ScrollRun],
                      value: &dyn Fn(&ScrollRun) -> Option<f64>,
                      unit: &str,
                      digits: usize| {
            let values: Vec<f64> = runs.iter().filter_map(value).collect();
            let (Some(low), Some(high)) = (
                values.iter().copied().reduce(f64::min),
                values.iter().copied().reduce(f64::max),
            ) else {
                return "—".to_owned();
            };
            if format!("{low:.digits$}") == format!("{high:.digits$}") {
                format!("{low:.digits$} {unit}")
            } else {
                format!("{low:.digits$}–{high:.digits$} {unit}")
            }
        };
        for name in SCENARIOS {
            let pair = [
                ("strips", run.scroll_runs(name)),
                (
                    "AppKit",
                    run.scrolls_appkit.get(name).map_or(&[][..], Vec::as_slice),
                ),
            ];
            for (drawing, runs) in pair {
                if runs.is_empty() {
                    continue;
                }
                let late: Vec<String> = runs.iter().map(|r| r.late.to_string()).collect();
                let _ = writeln!(
                    out,
                    "| `{name}`, {drawing} | {} / {} | {} | {} | {} | {} | {} |",
                    spread(runs, &|r| r.cpu_p50_ms, "ms", 2),
                    spread(runs, &|r| r.cpu_p99_ms, "ms", 2),
                    spread(runs, &|r| Some(r.instructions_mean), "M", 1),
                    spread(runs, &|r| r.other_threads_ms, "ms", 2),
                    spread(runs, &|r| r.window_server_ms, "ms", 2),
                    late.join(", "),
                    spread(runs, &|r| r.footprint_peak_mb, "MB", 0),
                );
            }
        }
    }
    out
}

// MARK: Runs

/// One launch with no document: the launch time and idle memory.
fn launch_run(options: &Options, log: &LogStream) -> Result<HashMap<String, f64>, String> {
    let launched = Launched::open(&options.app, None, &[])?;
    let result = (|| {
        let timeline = log
            .wait_for(launched.pid, Duration::from_secs(30), |t| {
                t.event("Launched").is_some()
            })
            .ok_or("the app didn't signal \"Launched\" within 30 s")?;
        let event = timeline.event("Launched").ok_or("no \"Launched\"")?;
        thread::sleep(options.settle);
        let heap = heap_report(launched.pid)?;
        let mut m = HashMap::new();
        m.insert(
            "launchedAfterMs".into(),
            launched_after_ms(event).ok_or("the \"Launched\" signpost has no time")?,
        );
        m.insert(
            "openCommandToLaunchedMs".into(),
            (event.time - launched.started) * 1e3,
        );
        m.insert("footprintMB".into(), heap.footprint_mb);
        m.insert("heapMB".into(), heap.heap_mb());
        m.insert("residentMB".into(), resident_mb(launched.pid)?);
        Ok(m)
    })();
    launched.quit();
    result
}

/// One launch with the reference file: open, index and the heap.
fn open_run(options: &Options, log: &LogStream) -> Result<HashMap<String, f64>, String> {
    let launched = Launched::open(&options.app, Some(&options.file), &[])?;
    let result = (|| {
        let pid = launched.pid;
        let timeline = log
            .wait_for(pid, Duration::from_secs(60), |t| {
                t.interval("Open to first rows").is_some()
                    && t.interval("Index").is_some()
                    && t.interval("Review").is_some()
                    && t.event("Launched").is_some()
            })
            .ok_or("the app didn't open the file, index it and review it within 60 s")?;
        thread::sleep(options.settle);
        let heap = heap_report(pid)?;
        let launched_event = timeline.event("Launched").ok_or("no \"Launched\"")?;
        let (_, first_rows) = timeline
            .interval("Open to first rows")
            .ok_or("no \"Open to first rows\"")?;
        let process_start =
            launched_event.time - launched_after_ms(launched_event).unwrap_or(0.0) / 1e3;
        let mut m = HashMap::new();
        let mut put = |k: &str, v: Option<f64>| {
            if let Some(v) = v {
                m.insert(k.to_owned(), v);
            }
        };
        put(
            "openToFirstRowsMs",
            timeline.duration_ms("Open to first rows"),
        );
        put("firstPaintMs", timeline.duration_ms("First paint"));
        put("indexMs", timeline.duration_ms("Index"));
        put("reviewMs", timeline.duration_ms("Review"));
        put(
            "launchToFirstRowsMs",
            Some((first_rows - process_start) * 1e3),
        );
        put("heapMB", Some(heap.heap_mb()));
        put("footprintMB", Some(heap.footprint_mb));
        put("footprintPeakMB", Some(heap.footprint_peak_mb));
        Ok(m)
    })();
    launched.quit();
    result
}

/// Opens in a running app: the bench build opens the reference file, then
/// closes it and opens it again five times (`-LealReopen`). The first open
/// is the cold one `open_run` measures; the others are returned.
fn reopen_run(options: &Options, log: &LogStream) -> Result<Vec<f64>, String> {
    const REOPENS: usize = 5;
    let args = ["-LealReopen".to_owned(), REOPENS.to_string()];
    let launched = Launched::open(&options.bench_app, Some(&options.file), &args)?;
    let timeline = log.wait_for(launched.pid, Duration::from_secs(120), |t| {
        t.durations_ms("Open to first rows").len() > REOPENS
    });
    let deadline = Instant::now() + Duration::from_secs(10);
    while launched.is_running() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(100));
    }
    launched.quit();
    let opens = timeline
        .ok_or("the bench build didn't reopen the file five times within 120 s")?
        .durations_ms("Open to first rows");
    Ok(opens.into_iter().skip(1).collect())
}

/// One edit run (task 2.5.1): the bench build opens the reference file,
/// edits `EDITS` cells through the in-cell editor, one at a time, and
/// quits. Each edit's "Cell edit to screen" signpost is one time.
fn edit_run(options: &Options, log: &LogStream) -> Result<Vec<f64>, String> {
    const EDITS: usize = 20;
    let args = ["-LealBenchEdit".to_owned(), EDITS.to_string()];
    let launched = Launched::open(&options.bench_app, Some(&options.file), &args)?;
    let timeline = log.wait_for(launched.pid, Duration::from_secs(120), |t| {
        t.durations_ms("Cell edit to screen").len() >= EDITS
    });
    let deadline = Instant::now() + Duration::from_secs(10);
    while launched.is_running() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(100));
    }
    launched.quit();
    Ok(timeline
        .ok_or("the bench build didn't edit 20 cells within 120 s")?
        .durations_ms("Cell edit to screen"))
}

/// One scroll benchmark run: the bench build opens `file` and scrolls
/// itself, writes its JSON in its container and quits.
fn scroll_run(options: &Options, file: &Path, extra: &[String]) -> Result<Value, String> {
    let home = std::env::var("HOME").map_err(|e| format!("HOME: {e}"))?;
    let name = format!("leal-perf-{}-{}.json", std::process::id(), now_nanos());
    let out = Path::new(&home).join(CONTAINER_TMP).join(&name);
    // A locked Mac turns its display off, and the display link stops with
    // it: wake it, and keep it on while the run lasts (`just bench-scroll`).
    let _ = Command::new("caffeinate").args(["-u", "-t", "2"]).status();
    let mut awake = Command::new("caffeinate")
        .arg("-d")
        .spawn()
        .map_err(|e| format!("caffeinate: {e}"))?;
    let mut args: Vec<String> = vec![
        "-LealBenchScroll".into(),
        name,
        "-LealBenchSpeed".into(),
        options.speed.clone(),
    ];
    args.extend(extra.iter().cloned());
    let result = (|| {
        let launched = Launched::open(&options.bench_app, Some(file), &args)?;
        let deadline = Instant::now() + Duration::from_secs(480);
        // WindowServer's CPU time, every 250 ms while the run lasts: the
        // render server composites the strips (task 2.0b).
        let server = window_server_pid();
        let mut samples: Vec<(f64, f64)> = Vec::new();
        while launched.is_running() {
            if Instant::now() > deadline {
                launched.quit();
                return Err("the scroll benchmark didn't finish in 480 s".to_owned());
            }
            if let Some(cpu) = server.and_then(process_cpu_seconds) {
                samples.push((now_seconds(), cpu));
            }
            thread::sleep(Duration::from_millis(250));
        }
        let text = std::fs::read_to_string(&out).map_err(|e| {
            format!(
                "the scroll benchmark wrote no results ({}): {e}",
                out.display()
            )
        })?;
        let _ = std::fs::remove_file(&out);
        let mut json: Value =
            serde_json::from_str(&text).map_err(|e| format!("{}: {e}", out.display()))?;
        let span = (
            json["scrollStartedAt"].as_f64(),
            json["scrollEndedAt"].as_f64(),
            json["scroll"]["frames"].as_f64(),
        );
        if let (Some(start), Some(end), Some(frames)) = span
            && frames > 0.0
            && let Some(cpu) = cpu_between(&samples, start, end)
        {
            json["windowServerCPUPerFrameMs"] = json!(cpu * 1000.0 / frames);
            json["windowServerCPUPercent"] = json!(cpu * 100.0 / (end - start));
        }
        Ok(json)
    })();
    let _ = awake.kill();
    let _ = awake.wait();
    result
}

// MARK: Processes

/// A Leal this run started with `open`, known by its PID.
struct Launched {
    pid: u32,
    /// Its executable, to check that the PID is still this process.
    executable: PathBuf,
    /// When `open` was called, in seconds since the epoch.
    started: f64,
}

impl Launched {
    /// Opens `app` (with `file`, and `args` as its launch arguments) as a
    /// new instance in front, as Finder would, and finds its PID: the one
    /// process of that executable that wasn't running before.
    fn open(app: &Path, file: Option<&Path>, args: &[String]) -> Result<Launched, String> {
        let executable = app.join("Contents/MacOS/Leal");
        let before = pids_of(&executable);
        let started = now_seconds();
        let mut command = Command::new("open");
        command.arg("-n").arg("-a").arg(app);
        if let Some(file) = file {
            command.arg(file);
        }
        command
            .args(["--args", "-ApplePersistenceIgnoreState", "YES"])
            .args(args);
        let status = command.status().map_err(|e| format!("open: {e}"))?;
        if !status.success() {
            return Err(format!("open {} failed: {status}", app.display()));
        }
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if let Some(pid) = pids_of(&executable)
                .into_iter()
                .find(|p| !before.contains(p))
            {
                return Ok(Launched {
                    pid,
                    executable,
                    started,
                });
            }
            if Instant::now() > deadline {
                return Err(format!("{} didn't start within 15 s", app.display()));
            }
            thread::sleep(Duration::from_millis(5));
        }
    }

    /// Whether the process is still running, and still this app: once it
    /// has exited, its PID could be another process's.
    fn is_running(&self) -> bool {
        Command::new("ps")
            .args(["-o", "command=", "-p", &self.pid.to_string()])
            .output()
            .is_ok_and(|o| {
                String::from_utf8_lossy(&o.stdout).contains(&*self.executable.to_string_lossy())
            })
    }

    /// Quits it (SIGTERM, then SIGKILL after 5 s) and waits until it has
    /// gone. The next launch removes any temporary folder it leaves.
    fn quit(&self) {
        if !self.is_running() {
            return;
        }
        let pid = self.pid.to_string();
        let _ = Command::new("kill")
            .args(["-TERM", &pid])
            .stderr(Stdio::null())
            .status();
        let deadline = Instant::now() + Duration::from_secs(5);
        while self.is_running() {
            if Instant::now() > deadline {
                let _ = Command::new("kill")
                    .args(["-KILL", &pid])
                    .stderr(Stdio::null())
                    .status();
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
        // LaunchServices notices the exit a moment later.
        thread::sleep(Duration::from_millis(500));
    }
}

fn pids_of(executable: &Path) -> Vec<u32> {
    let output = Command::new("pgrep")
        .arg("-f")
        .arg(executable)
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default();
    output
        .lines()
        .filter_map(|l| l.trim().parse().ok())
        .collect()
}

/// WindowServer's PID, if it can be found.
fn window_server_pid() -> Option<u32> {
    let output = Command::new("pgrep")
        .args(["-x", "WindowServer"])
        .output()
        .ok()?;
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .find_map(|l| l.trim().parse().ok())
}

/// A process's CPU time so far, in seconds, from `ps` (which can read it
/// for another user's process, such as WindowServer's).
fn process_cpu_seconds(pid: u32) -> Option<f64> {
    let output = Command::new("ps")
        .args(["-o", "time=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    parse_ps_time(&String::from_utf8_lossy(&output.stdout))
}

fn heap_report(pid: u32) -> Result<HeapReport, String> {
    let output = Command::new("heap")
        .args(["-s", &pid.to_string()])
        .output()
        .map_err(|e| format!("heap: {e}"))?;
    HeapReport::parse(&String::from_utf8_lossy(&output.stdout)).ok_or_else(|| {
        format!(
            "couldn't read `heap -s {pid}`: {}",
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

fn resident_mb(pid: u32) -> Result<f64, String> {
    let output = Command::new("ps")
        .args(["-o", "rss=", "-p", &pid.to_string()])
        .output()
        .map_err(|e| format!("ps: {e}"))?;
    let kb: f64 = String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse()
        .map_err(|e| format!("ps rss: {e}"))?;
    Ok(kb / 1024.0)
}

fn now_seconds() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64())
}

fn now_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos())
}

// MARK: Signposts

/// `log stream`, collecting Leal's signposts (the app's and the core's)
/// from every process as they arrive.
struct LogStream {
    child: Child,
    signposts: Arc<Mutex<Vec<Signpost>>>,
}

impl LogStream {
    fn start() -> Result<LogStream, String> {
        let mut child = Command::new("log")
            .args([
                "stream",
                "--signpost",
                "--style",
                "ndjson",
                "--predicate",
                "subsystem == \"io.github.robhaswell.leal\"",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("log stream: {e}"))?;
        let stdout = child.stdout.take().ok_or("log stream has no output")?;
        let signposts = Arc::new(Mutex::new(Vec::new()));
        let (ready, started) = mpsc::channel();
        let sink = Arc::clone(&signposts);
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                let _ = ready.send(());
                if let Some(signpost) = Signpost::parse(&line) {
                    sink.lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .push(signpost);
                }
            }
        });
        // Its first line ("Filtering the log data…") says it is streaming.
        started
            .recv_timeout(Duration::from_secs(10))
            .map_err(|_| "log stream didn't start within 10 s")?;
        thread::sleep(Duration::from_millis(200));
        Ok(LogStream { child, signposts })
    }

    /// Waits until `done` holds for `pid`'s signposts, and returns them.
    fn wait_for(
        &self,
        pid: u32,
        timeout: Duration,
        done: impl Fn(&Timeline) -> bool,
    ) -> Option<Timeline> {
        let deadline = Instant::now() + timeout;
        loop {
            let timeline = {
                let all = self
                    .signposts
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                Timeline::of(&all, pid)
            };
            if done(&timeline) {
                return Some(timeline);
            }
            if Instant::now() > deadline {
                return None;
            }
            thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for LogStream {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// MARK: The machine

/// The machine and its state, for the report (DESIGN §1's numbers are for
/// a base M1 Air; this records what they were measured on instead).
fn environment() -> Value {
    let text = |program: &str, args: &[&str]| {
        Command::new(program)
            .args(args)
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
            .unwrap_or_default()
    };
    let model = text("sysctl", &["-n", "hw.model"]);
    let cpu = text("sysctl", &["-n", "machdep.cpu.brand_string"]);
    let performance = text("sysctl", &["-n", "hw.perflevel0.physicalcpu"]);
    let efficiency = text("sysctl", &["-n", "hw.perflevel1.physicalcpu"]);
    let memory_gb = text("sysctl", &["-n", "hw.memsize"])
        .parse::<f64>()
        .map_or(0.0, |b| b / 1_073_741_824.0);
    let os = text("sw_vers", &["-productVersion"]);
    let power = text("pmset", &["-g", "batt"])
        .lines()
        .next()
        .unwrap_or_default()
        .to_owned();
    let low_power = text("pmset", &["-g"])
        .lines()
        .find(|l| l.contains("lowpowermode") || l.contains("powermode"))
        .map(|l| l.split_whitespace().collect::<Vec<_>>().join(" "))
        .unwrap_or_default();
    let load = text("sysctl", &["-n", "vm.loadavg"]);
    let processes = text("ps", &["-A", "-o", "pid="]).lines().count();
    let locked = text("ioreg", &["-n", "Root", "-d1"])
        .lines()
        .find(|l| l.contains("IOConsoleLocked"))
        .map_or("unknown", |l| {
            if l.contains("Yes") {
                "locked"
            } else {
                "unlocked"
            }
        })
        .to_owned();
    let displays: Vec<String> = text("system_profiler", &["SPDisplaysDataType"])
        .lines()
        .map(str::trim)
        .filter(|l| {
            l.starts_with("Display Type:")
                || l.starts_with("Resolution:")
                || l.starts_with("UI Looks like:")
                || l.starts_with("Main Display:")
        })
        .map(str::to_owned)
        .collect();
    let lines = vec![
        format!(
            "Machine: {model}, {cpu} ({performance} performance + {efficiency} efficiency cores), {memory_gb:.0} GB, macOS {os}"
        ),
        format!("Power: {power}; {low_power}"),
        format!("Load average: {load}; {processes} processes"),
        format!("Screen: {locked}"),
        format!("Displays: {}", displays.join("; ")),
    ];
    json!({
        "summary": format!("{model}, {cpu}, load {load}, screen {locked}"),
        "model": model,
        "lines": lines,
    })
}
