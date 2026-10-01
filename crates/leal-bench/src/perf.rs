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
        }
    }
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
    /// The main thread's CPU time per frame, 50th percentile, in ms.
    pub cpu_p50_ms: f64,
    /// The same, 99th percentile.
    pub cpu_p99_ms: f64,
    /// Frames whose main-thread work took longer than a 120 Hz refresh
    /// (8.3 ms): what decides the budget on a 60 Hz display.
    pub busy_over_120hz: u64,
    /// The main thread's instructions per frame, mean, in millions.
    pub instructions_mean: f64,
    /// The main thread's clock: cycles over CPU time, in GHz.
    pub ghz: f64,
    /// The heap's peak during the run, in MB.
    pub heap_peak_mb: f64,
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
        let number = |v: &Value, key: &str| v.get(key).and_then(Value::as_f64).unwrap_or(0.0);
        let count = |v: &Value, key: &str| v.get(key).and_then(Value::as_u64).unwrap_or(0);
        let background = |key: &str| {
            json.get(key)
                .map_or((0, 0), |v| (count(v, "late"), count(v, "frames")))
        };
        Some(ScrollRun {
            frames: scroll.get("frames")?.as_u64()?,
            late: scroll.get("late")?.as_u64()?,
            p99_ms: number(scroll, "p99"),
            cpu_p50_ms: number(scroll, "cpuP50"),
            cpu_p99_ms: number(scroll, "cpuP99"),
            busy_over_120hz: count(scroll, "busyOver120Hz"),
            instructions_mean: number(scroll, "instructionsMean"),
            ghz: number(scroll, "mainThreadGHz"),
            heap_peak_mb: number(json, "heapPeakMB"),
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
}

/// The table `just perf` prints, in Markdown.
#[must_use]
pub fn table(rows: &[Row]) -> String {
    let mut out = String::from(
        "| Budget (DESIGN §1) | Measured on this Mac | How | Verdict |\n|---|---|---|---|\n",
    );
    for row in rows {
        let _ = writeln!(
            out,
            "| {} | {} | {} | {} |",
            row.budget,
            row.measured,
            row.how,
            row.verdict.label()
        );
    }
    out
}

/// The scroll budget's verdict over several runs: no late frames at all.
#[must_use]
pub fn scroll_verdict(runs: &[ScrollRun]) -> Verdict {
    if runs.is_empty() {
        Verdict::Untested
    } else if runs.iter().all(|r| r.late == 0) {
        Verdict::Pass
    } else {
        Verdict::Fail
    }
}

/// Values of `key` from each map that has it.
#[must_use]
pub fn collect(maps: &[HashMap<String, f64>], key: &str) -> Vec<f64> {
    maps.iter().filter_map(|m| m.get(key).copied()).collect()
}
