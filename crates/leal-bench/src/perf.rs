//! The app's DESIGN §1 budgets, measured on the app as it ships (task
//! 1.10): the parts of `leal-perf` that don't launch anything, so they can be
//! tested. `leal-perf` launches the app with `open`, reads its signposts
//! from `log stream`, its memory from `heap`, and the scroll benchmark's
//! JSON; this module turns those into numbers and the table `just perf`
//! prints (`docs/perf.md` explains each row).

use std::collections::HashMap;
use std::fmt::Write as _;

use serde_json::Value;

/// What a signpost marks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SignpostKind {
    /// The start of an interval.
    Begin,
    /// The end of an interval.
    End,
    /// A single moment.
    Event,
}

/// One signpost from `log stream --signpost --style ndjson`.
#[derive(Clone, Debug, PartialEq)]
pub struct Signpost {
    /// The process that emitted it.
    pub pid: u32,
    /// Its name, such as "First paint".
    pub name: String,
    /// Begin, end or event.
    pub kind: SignpostKind,
    /// Pairs an interval's begin with its end.
    pub id: u64,
    /// When, in seconds since the Unix epoch.
    pub time: f64,
    /// The message, for an event that has one.
    pub message: String,
}

impl Signpost {
    /// Reads one line of `log stream --style ndjson` output. Lines that
    /// aren't signposts (the stream's header, other log messages) give
    /// `None`.
    #[must_use]
    pub fn parse(line: &str) -> Option<Signpost> {
        let value: Value = serde_json::from_str(line).ok()?;
        if value.get("eventType")?.as_str()? != "signpostEvent" {
            return None;
        }
        let kind = match value.get("signpostType")?.as_str()? {
            "begin" => SignpostKind::Begin,
            "end" => SignpostKind::End,
            "event" => SignpostKind::Event,
            _ => return None,
        };
        Some(Signpost {
            pid: u32::try_from(value.get("processID")?.as_u64()?).ok()?,
            name: value.get("signpostName")?.as_str()?.to_owned(),
            kind,
            id: value.get("signpostID")?.as_u64()?,
            time: parse_timestamp(value.get("timestamp")?.as_str()?)?,
            message: value
                .get("eventMessage")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
        })
    }
}

/// Seconds since the Unix epoch for a `log` timestamp such as
/// `2026-10-01 21:04:09.317471+0100` (local time and its offset from UTC).
#[must_use]
pub fn parse_timestamp(text: &str) -> Option<f64> {
    let (date, rest) = text.split_once(' ')?;
    let mut parts = date.splitn(3, '-');
    let year: i64 = parts.next()?.parse().ok()?;
    let month: i64 = parts.next()?.parse().ok()?;
    let day: i64 = parts.next()?.parse().ok()?;
    // The offset is the last five characters: a sign and HHMM.
    let split = rest.len().checked_sub(5)?;
    let (time, offset) = rest.split_at(split);
    let sign = match offset.as_bytes().first()? {
        b'+' => 1,
        b'-' => -1,
        _ => return None,
    };
    let offset_hours: i64 = offset.get(1..3)?.parse().ok()?;
    let offset_minutes: i64 = offset.get(3..5)?.parse().ok()?;
    let mut clock = time.splitn(3, ':');
    let hours: i64 = clock.next()?.parse().ok()?;
    let minutes: i64 = clock.next()?.parse().ok()?;
    let seconds: f64 = clock.next()?.parse().ok()?;
    let days = days_from_civil(year, month, day)?;
    let whole = days * 86_400 + hours * 3_600 + minutes * 60
        - sign * (offset_hours * 3_600 + offset_minutes * 60);
    Some(whole as f64 + seconds)
}

/// Days since 1970-01-01 of a date in the proleptic Gregorian calendar
/// (Howard Hinnant's `days_from_civil`).
fn days_from_civil(year: i64, month: i64, day: i64) -> Option<i64> {
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let month_from_march = (month + 9) % 12;
    let day_of_year = (153 * month_from_march + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    Some(era * 146_097 + day_of_era - 719_468)
}

/// The signposts of one process.
#[derive(Debug, Default)]
pub struct Timeline {
    signposts: Vec<Signpost>,
}

impl Timeline {
    /// `pid`'s signposts among `all`.
    #[must_use]
    pub fn of(all: &[Signpost], pid: u32) -> Timeline {
        Timeline {
            signposts: all.iter().filter(|s| s.pid == pid).cloned().collect(),
        }
    }

    /// The first event called `name`.
    #[must_use]
    pub fn event(&self, name: &str) -> Option<&Signpost> {
        self.signposts
            .iter()
            .find(|s| s.kind == SignpostKind::Event && s.name == name)
    }

    /// The first complete interval called `name`: its begin and end times.
    #[must_use]
    pub fn interval(&self, name: &str) -> Option<(f64, f64)> {
        let begin = self
            .signposts
            .iter()
            .find(|s| s.kind == SignpostKind::Begin && s.name == name)?;
        let end = self
            .signposts
            .iter()
            .find(|s| s.kind == SignpostKind::End && s.name == name && s.id == begin.id)?;
        Some((begin.time, end.time))
    }

    /// Every complete interval called `name`, in milliseconds, in the
    /// order they began.
    #[must_use]
    pub fn durations_ms(&self, name: &str) -> Vec<f64> {
        self.signposts
            .iter()
            .filter(|s| s.kind == SignpostKind::Begin && s.name == name)
            .filter_map(|begin| {
                let end = self
                    .signposts
                    .iter()
                    .find(|s| s.kind == SignpostKind::End && s.name == name && s.id == begin.id)?;
                Some((end.time - begin.time) * 1e3)
            })
            .collect()
    }

    /// The first complete interval called `name`, in milliseconds.
    #[must_use]
    pub fn duration_ms(&self, name: &str) -> Option<f64> {
        self.interval(name).map(|(b, e)| (e - b) * 1e3)
    }
}

/// The milliseconds in a "Launched" event's message (`252.335 ms after the
/// process started`).
#[must_use]
pub fn launched_after_ms(event: &Signpost) -> Option<f64> {
    event.message.split_whitespace().next()?.parse().ok()
}

/// What `heap -s` says about a process.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct HeapReport {
    /// Bytes in use in every malloc zone: Leal's own heap (DESIGN §1).
    pub heap_bytes: u64,
    /// The physical footprint, in MB: everything macOS charges to the
    /// process (Activity Monitor's "Memory").
    pub footprint_mb: f64,
    /// The highest footprint so far, in MB.
    pub footprint_peak_mb: f64,
}

impl HeapReport {
    /// Reads `heap -s <pid>`'s output.
    #[must_use]
    pub fn parse(text: &str) -> Option<HeapReport> {
        let mut report = HeapReport::default();
        let mut found = (false, false);
        for line in text.lines() {
            let line = line.trim();
            if let Some(rest) = line.strip_prefix("All zones:")
                && let Some(bytes) = rest
                    .split_once('(')
                    .and_then(|(_, b)| b.split_whitespace().next())
            {
                report.heap_bytes = bytes.parse().ok()?;
                found.0 = true;
            } else if let Some(rest) = line.strip_prefix("Physical footprint (peak):") {
                report.footprint_peak_mb = parse_size_mb(rest.trim())?;
            } else if let Some(rest) = line.strip_prefix("Physical footprint:") {
                report.footprint_mb = parse_size_mb(rest.trim())?;
                found.1 = true;
            }
        }
        (found.0 && found.1).then_some(report)
    }

    /// The heap in MB (2^20 bytes).
    #[must_use]
    pub fn heap_mb(&self) -> f64 {
        let bytes = self.heap_bytes as f64;
        bytes / 1_048_576.0
    }
}

/// `48.3M`, `512K` or `1.2G` (as `heap` and `vmmap` print sizes) in MB.
fn parse_size_mb(text: &str) -> Option<f64> {
    let (number, unit) = text.split_at(text.len().checked_sub(1)?);
    let value: f64 = number.parse().ok()?;
    match unit {
        "K" => Some(value / 1024.0),
        "M" => Some(value),
        "G" => Some(value * 1024.0),
        _ => None,
    }
}

/// The median, smallest and largest of some runs' values.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Spread {
    /// The middle value (the mean of the middle two for an even count).
    pub median: f64,
    /// The smallest.
    pub min: f64,
    /// The largest.
    pub max: f64,
    /// How many runs.
    pub runs: usize,
}

impl Spread {
    /// `None` for no values.
    #[must_use]
    pub fn of(values: &[f64]) -> Option<Spread> {
        if values.is_empty() {
            return None;
        }
        let mut sorted = values.to_vec();
        sorted.sort_by(f64::total_cmp);
        let middle = sorted.len() / 2;
        let median = if sorted.len() % 2 == 1 {
            sorted[middle]
        } else {
            f64::midpoint(sorted[middle - 1], sorted[middle])
        };
        Some(Spread {
            median,
            min: sorted[0],
            max: sorted[sorted.len() - 1],
            runs: sorted.len(),
        })
    }

    /// `12.3 ms (11.9–13.0, 3 runs)`, with `unit` and `decimals`.
    #[must_use]
    pub fn describe(&self, unit: &str, decimals: usize) -> String {
        if self.runs == 1 {
            return format!("{:.decimals$} {unit} (1 run)", self.median);
        }
        format!(
            "{:.decimals$} {unit} ({:.decimals$}–{:.decimals$}, {} runs)",
            self.median, self.min, self.max, self.runs
        )
    }
}

/// A budget's verdict.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Every run was inside the budget.
    Pass,
    /// The median was inside it but at least one run wasn't.
    Mixed,
    /// The median was outside it.
    Fail,
    /// Not measured.
    Untested,
    /// The heap was inside the budget where it was measured, but never
    /// with a search's results held: a `--no-scroll` run, or a run saved
    /// before the scroll benchmark's JSON had `heapSettledMB`.
    OpenOnly,
}

impl Verdict {
    /// The verdict for runs of a "less than `budget`" metric.
    #[must_use]
    pub fn below(spread: Option<Spread>, budget: f64) -> Verdict {
        match spread {
            None => Verdict::Untested,
            Some(s) if s.max < budget => Verdict::Pass,
            Some(s) if s.median < budget => Verdict::Mixed,
            Some(_) => Verdict::Fail,
        }
    }

    /// How the table shows it.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Verdict::Pass => "pass",
            Verdict::Mixed => "mixed",
            Verdict::Fail => "fail",
            Verdict::Untested => "untested",
            Verdict::OpenOnly => "pass (search results untested)",
        }
    }

    /// Two checks of one budget together: a fail in either fails it, and
    /// both must pass for a pass. An untested check is left out. A partial
    /// pass ([`Verdict::OpenOnly`]) stays partial beside a pass.
    #[must_use]
    pub fn and(self, other: Verdict) -> Verdict {
        match (self, other) {
            (Verdict::Untested, v) | (v, Verdict::Untested) => v,
            (Verdict::Fail, _) | (_, Verdict::Fail) => Verdict::Fail,
            (Verdict::Mixed, _) | (_, Verdict::Mixed) => Verdict::Mixed,
            (Verdict::OpenOnly, Verdict::Pass | Verdict::OpenOnly)
            | (Verdict::Pass, Verdict::OpenOnly) => Verdict::OpenOnly,
            (Verdict::Pass, Verdict::Pass) => Verdict::Pass,
        }
    }
}

/// The per-window AppKit baseline, in MB, that DESIGN §1 leaves out of
/// the heap budget: what AppKit and Core Animation allocate for a document
/// window. Measured in task 1.10 as the heap with a two-row file open,
/// 21 MB (against 2.5 MB with no window), on the M5 Pro with the screen
/// locked, 1 October 2026 (docs/tasks/1.10.md, "Heap peak"). `leal-perf`
/// doesn't measure it itself; measure it again if the window changes much.
pub const APPKIT_WINDOW_HEAP_MB: f64 = 21.0;

/// DESIGN §1's 3× rule: on a Mac faster than the reference machine, the
/// main thread's work per scroll frame must be at most this. It is about a
/// third of a 120 Hz frame (8.3 ms), so a base M1 Air, expected to be 2–3
/// times slower, would still fit in one. It is checked at p50 and p99
/// (p50 and p99: proposed, awaiting Rob), since no dropped frames makes
/// p99 the honest reading.
pub const HEADROOM_FRAME_MS: f64 = 2.8;

/// The reference machine's `hw.model`: the base M1 MacBook Air (DESIGN
/// §1). The 3× rule stands in for it, so it doesn't apply on it.
pub const REFERENCE_MODEL: &str = "MacBookAir10,1";

/// Whether `model` (`hw.model`) is the reference machine.
#[must_use]
pub fn is_reference_machine(model: &str) -> bool {
    model == REFERENCE_MODEL
}

/// A run's `hw.model`, from its environment: the `model` field, or for a
/// run saved before that was recorded, the start of its "Machine: …" line
/// (`Machine: Mac17,8, Apple M5 Pro (…)`). Empty if neither is there.
#[must_use]
pub fn machine_model(environment: &Value) -> String {
    if let Some(model) = environment["model"].as_str() {
        return model.to_owned();
    }
    environment["lines"]
        .as_array()
        .and_then(|lines| lines.first())
        .and_then(Value::as_str)
        .and_then(|line| line.strip_prefix("Machine: "))
        .map(|rest| rest.split_once(", ").map_or(rest, |(model, _)| model))
        .unwrap_or_default()
        .trim()
        .to_owned()
}

/// One scroll benchmark run's figures, from its JSON (`ScrollBench`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ScrollRun {
    /// Frames recorded while scrolling (every stage but the jumps).
    pub frames: u64,
    /// Frames that missed at least one refresh.
    pub late: u64,
    /// The frame interval's 99th percentile, in ms.
    pub p99_ms: f64,
    /// The main thread's CPU time per frame, 50th percentile, in ms, if
    /// the run reported it.
    pub cpu_p50_ms: Option<f64>,
    /// The same, 99th percentile.
    pub cpu_p99_ms: Option<f64>,
    /// Frames whose main-thread work took longer than a 120 Hz refresh
    /// (8.3 ms): what decides the budget on a 60 Hz display.
    pub busy_over_120hz: u64,
    /// The main thread's instructions per frame, mean, in millions.
    pub instructions_mean: f64,
    /// The main thread's clock: cycles over CPU time, in GHz.
    pub ghz: f64,
    /// The heap's peak during the run, in MB.
    pub heap_peak_mb: f64,
    /// The heap once the run had settled after scrolling, in MB, with any
    /// search's results still held, if the run reported it.
    pub heap_settled_mb: Option<f64>,
    /// The screen's highest refresh rate.
    pub screen_fps: f64,
    /// Late frames and all frames while the index ran.
    pub while_indexing: (u64, u64),
    /// Late frames and all frames while a search ran.
    pub while_finding: (u64, u64),
    /// Searches started.
    pub find_runs: u64,
    /// The display link stopped for more than half a second (the display
    /// slept): the run doesn't count.
    pub stalls: u64,
    /// The window was visible at the end.
    pub visible: bool,
}

impl ScrollRun {
    /// Reads a run's JSON.
    #[must_use]
    pub fn parse(json: &Value) -> Option<ScrollRun> {
        let scroll = json.get("scroll")?;
        let optional = |v: &Value, key: &str| v.get(key).and_then(Value::as_f64);
        let number = |v: &Value, key: &str| optional(v, key).unwrap_or(0.0);
        let count = |v: &Value, key: &str| v.get(key).and_then(Value::as_u64).unwrap_or(0);
        let background = |key: &str| {
            json.get(key)
                .map_or((0, 0), |v| (count(v, "late"), count(v, "frames")))
        };
        Some(ScrollRun {
            frames: scroll.get("frames")?.as_u64()?,
            late: scroll.get("late")?.as_u64()?,
            p99_ms: number(scroll, "p99"),
            cpu_p50_ms: optional(scroll, "cpuP50"),
            cpu_p99_ms: optional(scroll, "cpuP99"),
            busy_over_120hz: count(scroll, "busyOver120Hz"),
            instructions_mean: number(scroll, "instructionsMean"),
            ghz: number(scroll, "mainThreadGHz"),
            heap_peak_mb: number(json, "heapPeakMB"),
            heap_settled_mb: optional(json, "heapSettledMB"),
            screen_fps: number(json, "screenMaxFPS"),
            while_indexing: background("whileIndexing"),
            while_finding: background("whileFinding"),
            find_runs: count(json, "findRuns"),
            stalls: count(json, "displayLinkStalls"),
            visible: json
                .get("windowVisible")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        })
    }

    /// The refresh rate below which a run is judged from its frame work
    /// rather than its late frames.
    pub const BUDGET_FPS: f64 = 120.0;

    /// Whether the run's display refreshed at 120 Hz, so its late frames
    /// are the budget's dropped frames. Below that (a base M1 Air's 60 Hz
    /// panel) a late frame is one that missed 16.7 ms, which says nothing
    /// about 120 Hz.
    #[must_use]
    pub fn at_budget_rate(&self) -> bool {
        self.screen_fps >= Self::BUDGET_FPS - 1.0
    }

    /// The frames that count as dropped at 120 Hz: the late ones on a
    /// 120 Hz display; on a slower one, those whose main-thread work alone
    /// took longer than 8.3 ms (they would have missed a 120 Hz refresh,
    /// though frames that missed it in rendering aren't counted).
    #[must_use]
    pub fn dropped_at_120hz(&self) -> u64 {
        if self.at_budget_rate() {
            self.late
        } else {
            self.busy_over_120hz
        }
    }

    /// The run as the table shows it: its late frames, or on a slower
    /// display its frames with more than 8.3 ms of work.
    #[must_use]
    pub fn describe(&self) -> String {
        if self.at_budget_rate() {
            Self::describe_late(self.late, self.frames)
        } else {
            format!(
                "{} of {} frames over 8.3 ms of main-thread work ({} Hz display)",
                thousands(self.busy_over_120hz),
                thousands(self.frames),
                self.screen_fps
            )
        }
    }

    /// Whether the main thread's work per frame was at most `limit_ms` at
    /// both p50 and p99 (the 3× rule, [`HEADROOM_FRAME_MS`]); `None` if the
    /// run didn't report both.
    #[must_use]
    pub fn within_headroom(&self, limit_ms: f64) -> Option<bool> {
        Some(self.cpu_p50_ms? <= limit_ms && self.cpu_p99_ms? <= limit_ms)
    }

    /// `2.9/5.8 ms`: the main thread's work per frame at p50 and p99, `—`
    /// for a figure the run didn't report.
    #[must_use]
    pub fn describe_cpu(&self) -> String {
        let figure = |v: Option<f64>| v.map_or("—".to_owned(), |v| format!("{v:.1}"));
        format!("{}/{} ms", figure(self.cpu_p50_ms), figure(self.cpu_p99_ms))
    }

    /// `3 of 6,150 late (0.05%)`.
    #[must_use]
    pub fn describe_late(late: u64, frames: u64) -> String {
        let percent = if frames == 0 {
            0.0
        } else {
            late as f64 * 100.0 / frames as f64
        };
        format!(
            "{} of {} late ({percent:.2}%)",
            thousands(late),
            thousands(frames)
        )
    }
}

/// `6150` as `6,150`.
#[must_use]
pub fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// One row of the budget table.
#[derive(Clone, Debug)]
pub struct Row {
    /// The budget, as DESIGN §1 words it.
    pub budget: String,
    /// What was measured.
    pub measured: String,
    /// How.
    pub how: String,
    /// Pass, fail or untested.
    pub verdict: Verdict,
    /// Why, when the verdict has more than one check: shown after it, in
    /// brackets.
    pub note: Option<&'static str>,
}

/// The table `just perf` prints, in Markdown.
#[must_use]
pub fn table(rows: &[Row]) -> String {
    let mut out = String::from(
        "| Budget (DESIGN §1) | Measured on this Mac | How | Verdict |\n|---|---|---|---|\n",
    );
    for row in rows {
        let note = row.note.map_or(String::new(), |note| format!(" ({note})"));
        let _ = writeln!(
            out,
            "| {} | {} | {} | {}{note} |",
            row.budget,
            row.measured,
            row.how,
            row.verdict.label()
        );
    }
    out
}

/// The scroll budget's late-frame verdict over several runs: no frame
/// dropped at 120 Hz in any ([`ScrollRun::dropped_at_120hz`]). The 3× rule
/// is [`headroom_verdict`].
#[must_use]
pub fn scroll_verdict(runs: &[ScrollRun]) -> Verdict {
    if runs.is_empty() {
        Verdict::Untested
    } else if runs.iter().all(|r| r.dropped_at_120hz() == 0) {
        Verdict::Pass
    } else {
        Verdict::Fail
    }
}

/// The 3× rule's verdict over several runs: every run's main-thread work
/// per frame at most `limit_ms` at p50 and p99
/// ([`ScrollRun::within_headroom`]). Any run over it fails; otherwise a
/// run without the figures leaves it untested, so it never passes on
/// missing numbers.
#[must_use]
pub fn headroom_verdict(runs: &[ScrollRun], limit_ms: f64) -> Verdict {
    let checks: Vec<Option<bool>> = runs.iter().map(|r| r.within_headroom(limit_ms)).collect();
    if checks.contains(&Some(false)) {
        Verdict::Fail
    } else if checks.is_empty() || checks.contains(&None) {
        Verdict::Untested
    } else {
        Verdict::Pass
    }
}

/// `p50 2.8–3.6 ms, p99 5.8–6.8 ms`: the main thread's work per frame over
/// some runs, from the runs that reported it (`—` for none).
#[must_use]
pub fn describe_frame_work(runs: &[ScrollRun]) -> String {
    let range = |values: Vec<f64>| {
        Spread::of(&values).map_or("—".to_owned(), |s| {
            if s.runs == 1 || (s.max - s.min).abs() < 0.05 {
                format!("{:.1} ms", s.median)
            } else {
                format!("{:.1}–{:.1} ms", s.min, s.max)
            }
        })
    };
    let p50: Vec<f64> = runs.iter().filter_map(|r| r.cpu_p50_ms).collect();
    let p99: Vec<f64> = runs.iter().filter_map(|r| r.cpu_p99_ms).collect();
    if p50.is_empty() && p99.is_empty() {
        return "—".to_owned();
    }
    format!("p50 {}, p99 {}", range(p50), range(p99))
}

/// Values of `key` from each map that has it.
#[must_use]
pub fn collect(maps: &[HashMap<String, f64>], key: &str) -> Vec<f64> {
    maps.iter().filter_map(|m| m.get(key).copied()).collect()
}

/// The scroll scenarios `leal-perf` runs, in the order the report lists
/// them.
pub const SCENARIOS: [&str; 3] = ["afterLoad", "duringLoadWithFind", "bigFileNoPause"];

/// A `leal-perf` run's numbers, read back from the JSON it saves.
#[derive(Clone, Debug, Default)]
pub struct PerfRun {
    /// The machine's `hw.model` ([`machine_model`]).
    pub model: String,
    /// Each launch with no document: `launchedAfterMs`, `footprintMB`, ….
    pub launches: Vec<HashMap<String, f64>>,
    /// Each launch with the reference file: `openToFirstRowsMs`, `heapMB`, ….
    pub opens: Vec<HashMap<String, f64>>,
    /// Each open in a running app, in ms.
    pub reopens_ms: Vec<f64>,
    /// Each scenario's scroll runs, by name ([`SCENARIOS`]).
    pub scrolls: HashMap<String, Vec<ScrollRun>>,
}

impl PerfRun {
    /// Reads the JSON `leal-perf` saves. Missing parts are left empty.
    #[must_use]
    pub fn parse(raw: &Value) -> PerfRun {
        let maps = |key: &str| -> Vec<HashMap<String, f64>> {
            raw[key]
                .as_array()
                .map(|runs| {
                    runs.iter()
                        .filter_map(Value::as_object)
                        .map(|m| {
                            m.iter()
                                .filter_map(|(k, v)| Some((k.clone(), v.as_f64()?)))
                                .collect()
                        })
                        .collect()
                })
                .unwrap_or_default()
        };
        let mut scrolls = HashMap::new();
        for name in SCENARIOS {
            let runs: Vec<ScrollRun> = raw["scrolls"][name]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(ScrollRun::parse)
                .collect();
            if !runs.is_empty() {
                scrolls.insert(name.to_owned(), runs);
            }
        }
        PerfRun {
            model: machine_model(&raw["environment"]),
            launches: maps("launches"),
            opens: maps("opens"),
            reopens_ms: raw["reopensMs"]
                .as_array()
                .map(|v| v.iter().filter_map(Value::as_f64).collect())
                .unwrap_or_default(),
            scrolls,
        }
    }

    /// A scenario's scroll runs (none if it wasn't run).
    #[must_use]
    pub fn scroll_runs(&self, name: &str) -> &[ScrollRun] {
        self.scrolls.get(name).map_or(&[], Vec::as_slice)
    }

    /// The budget table's rows, judged as DESIGN §1's notes say.
    #[must_use]
    pub fn rows(&self) -> Vec<Row> {
        let spread = |maps: &[HashMap<String, f64>], key: &str| Spread::of(&collect(maps, key));
        let ms = |s: Option<Spread>| s.map_or("—".to_owned(), |s| s.describe("ms", 1));
        let mb = |s: Option<Spread>| s.map_or("—".to_owned(), |s| s.describe("MB", 1));
        let launch = spread(&self.launches, "launchedAfterMs");
        let cold_launch = spread(&self.opens, "launchToFirstRowsMs");
        let reopen = Spread::of(&self.reopens_ms);
        let index = spread(&self.opens, "indexMs");
        let idle = spread(&self.launches, "footprintMB");
        let idle_rss = spread(&self.launches, "residentMB");
        let reference_machine = is_reference_machine(&self.model);
        vec![
            Row {
                budget: "Launch < 300 ms".into(),
                measured: ms(launch),
                how: "process start to the end of `applicationDidFinishLaunching` (\"Launched\" signpost); Leal opens no empty window".into(),
                verdict: Verdict::below(launch, 300.0),
                note: None,
            },
            Row {
                budget: "Launched with a file, to its first rows < 450 ms".into(),
                measured: ms(cold_launch),
                how: "process start to the grid's first draw with rows, reference file: the cold open, judged as part of launch against the launch and open budgets together".into(),
                verdict: Verdict::below(cold_launch, 450.0),
                note: None,
            },
            Row {
                budget: "Open to first rows < 150 ms".into(),
                measured: ms(reopen),
                how: "`read(from:)` to the grid's first draw with rows (\"Open to first rows\" signpost), reference file, closed and opened again in the running app: the warm open (bench build, `-LealReopen`)".into(),
                verdict: Verdict::below(reopen, 150.0),
                note: None,
            },
            Row {
                budget: "Full index < 500 ms".into(),
                measured: ms(index),
                how: "the core's \"Index\" signpost in the app, reference file, with diagnostics".into(),
                verdict: Verdict::below(index, 500.0),
                note: None,
            },
            scroll_row(
                self.scroll_runs("afterLoad"),
                "Scrolling: no dropped frames at 120 Hz",
                "`ScrollBench` flings, reference file, after indexing; late = missed a refresh",
                Headroom::of(reference_machine),
            ),
            scroll_row(
                self.scroll_runs("duringLoadWithFind"),
                "… including while background work runs",
                "the same from the first rows, while the index and review run, with a search running throughout",
                Headroom::of(reference_machine),
            ),
            scroll_row(
                self.scroll_runs("bigFileNoPause"),
                "… stress: background work not pausing (beyond the budget)",
                "1 GB variant from the first rows: index, review and a search all running, the scroll not reported as input; judged on late frames only",
                Headroom::NotJudged,
            ),
            heap_row(
                &self.opens,
                self.scroll_runs("afterLoad"),
                self.scroll_runs("duringLoadWithFind"),
            ),
            Row {
                budget: "Idle app, no document < 30 MB footprint".into(),
                measured: format!(
                    "{} footprint. Not judged: resident size {}",
                    mb(idle),
                    mb(idle_rss)
                ),
                how: "`heap -s` physical footprint (Activity Monitor's Memory) after the app settles; the resident size (RSS) also counts shared system libraries".into(),
                verdict: Verdict::below(idle, 30.0),
                note: None,
            },
        ]
    }
}

/// Whether a scroll row is judged on the 3× rule as well as late frames.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Headroom {
    /// A reference-file row on a faster Mac: the 3× rule applies.
    Judged,
    /// A reference-file row on the reference machine, which the rule
    /// stands in for.
    ReferenceMachine,
    /// The stress row, beyond the budget: late frames only.
    NotJudged,
}

impl Headroom {
    /// For a reference-file row.
    fn of(reference_machine: bool) -> Headroom {
        if reference_machine {
            Headroom::ReferenceMachine
        } else {
            Headroom::Judged
        }
    }
}

/// A scroll budget's row: late frames, and the 3× rule where it applies.
fn scroll_row(runs: &[ScrollRun], budget: &str, how: &str, headroom: Headroom) -> Row {
    let measured = if runs.is_empty() {
        "—".to_owned()
    } else {
        format!(
            "{}; main-thread work per frame {}",
            runs.iter()
                .map(ScrollRun::describe)
                .collect::<Vec<_>>()
                .join("; "),
            describe_frame_work(runs)
        )
    };
    // Say so when a run's display wasn't 120 Hz: its late-frame verdict
    // comes from frame work, not from frames seen to drop (docs/perf.md).
    let mut how = match runs.iter().find(|r| !r.at_budget_rate()) {
        Some(slow) => format!(
            "{how}. **120 Hz judged from frame work; the display is {} Hz**",
            slow.screen_fps
        ),
        None => how.to_owned(),
    };
    let late = scroll_verdict(runs);
    let (verdict, note) = match headroom {
        Headroom::Judged => {
            how.push_str(&format!(
                "; 3× rule (DESIGN §1): main-thread work per frame ≤ {HEADROOM_FRAME_MS} ms at p50 and p99 (p50 and p99: proposed, awaiting Rob)"
            ));
            match (late, headroom_verdict(runs, HEADROOM_FRAME_MS)) {
                (Verdict::Untested, _) => (Verdict::Untested, None),
                (Verdict::Fail, Verdict::Fail) => (Verdict::Fail, Some("late frames; 3× rule")),
                (Verdict::Fail, _) => (Verdict::Fail, Some("late frames")),
                (_, Verdict::Fail) => (Verdict::Fail, Some("3× rule")),
                // The runs didn't report their frame work: no pass.
                (_, Verdict::Untested) => (Verdict::Untested, Some("3× rule untested")),
                (late, rule) => (late.and(rule), None),
            }
        }
        Headroom::ReferenceMachine => {
            how.push_str("; the reference machine, so the 3× rule doesn't apply");
            (late, None)
        }
        Headroom::NotJudged => (late, None),
    };
    Row {
        budget: budget.to_owned(),
        measured,
        how,
        verdict,
        note,
    }
}

/// The heap budget's row (DESIGN §1): every malloc zone minus the
/// per-window AppKit baseline, after opening and once settled after
/// scrolling, without a search (`without_search`) and with one's results
/// held (`with_search`). The peaks while scrolling are AppKit's drawing,
/// so they are reported but not judged.
fn heap_row(
    opens: &[HashMap<String, f64>],
    without_search: &[ScrollRun],
    with_search: &[ScrollRun],
) -> Row {
    let mb = |s: Option<Spread>| s.map_or("—".to_owned(), |s| s.describe("MB", 1));
    let leal_own = |values: Vec<f64>| -> Option<Spread> {
        let own: Vec<f64> = values.iter().map(|v| v - APPKIT_WINDOW_HEAP_MB).collect();
        Spread::of(&own)
    };
    let settled =
        |runs: &[ScrollRun]| leal_own(runs.iter().filter_map(|r| r.heap_settled_mb).collect());
    let after_open = leal_own(collect(opens, "heapMB"));
    let no_search = settled(without_search);
    let search = settled(with_search);
    let peaks: Vec<f64> = without_search
        .iter()
        .chain(with_search)
        .map(|r| r.heap_peak_mb)
        .filter(|&v| v > 0.0)
        .collect();
    let all_zones = Spread::of(&collect(opens, "heapMB"));
    let verdict = match (
        Verdict::below(after_open, 40.0).and(Verdict::below(no_search, 40.0)),
        Verdict::below(search, 40.0),
    ) {
        // Never measured with a search's results held.
        (Verdict::Pass, Verdict::Untested) => Verdict::OpenOnly,
        (others, with_search) => others.and(with_search),
    };
    Row {
        budget: "Leal's heap, reference file < 40 MB".into(),
        measured: format!(
            "{} after opening; settled after scrolling, {} without a search and {} with a search's results held. Not judged, all zones: {} after opening; peak while scrolling {}",
            mb(after_open),
            mb(no_search),
            mb(search),
            mb(all_zones),
            mb(Spread::of(&peaks))
        ),
        how: format!(
            "all malloc zones minus the per-window AppKit baseline ({APPKIT_WINDOW_HEAP_MB} MB, a two-row file open: docs/tasks/1.10.md): `heap -s` after the review finished, and the bench's `malloc_zone_statistics` once settled after scrolling; AppKit's drawing peaks are left out"
        ),
        verdict,
        note: None,
    }
}
