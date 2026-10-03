//! Reads criterion's results and decides what CI reports and fails on.
//!
//! Criterion writes each benchmark's results to
//! `<criterion dir>/<benchmark>/new/` (`benchmark.json` for its id and
//! throughput, `estimates.json` for its statistics). When it compares with
//! a saved baseline (`--baseline-lenient base`), it also writes
//! `<benchmark>/change/estimates.json`: the relative change, with a 95%
//! confidence interval. This module reads those files; `bench-report`
//! prints the result.
//!
//! **A benchmark regressed** on an attempt if the lower end of the 95%
//! interval for its median's change is above [`Thresholds::regression`]
//! (+20% on CI). Requiring the whole interval to be above the threshold
//! means a single noisy median doesn't count. **A benchmark is over
//! budget** if its median is over its budget ([`crate::budgets`]), and a
//! budgeted benchmark with no result is **missing**. Both of those fail on
//! any attempt, noisy or not.
//!
//! # Noise: two kinds of canary
//!
//! A canary is a benchmark whose code is the same on both sides of every
//! comparison, so when one moves, the machine moved. A canary has moved if
//! its median changed, either way, by more than [`Thresholds::noise`] (10%
//! on CI).
//!
//! - **Run-wide canaries** ([`CANARIES`]): the `memchr3_scan` baselines,
//!   which run first, and again as `baseline-late` after all the others.
//!   If one moved, **the run is noisy** ([`Report::noisy`]).
//! - **Group canaries** ([`GROUP_CANARY`]): a short `memchr3` scan, with
//!   about 1 s of measurement, that runs just before and just after each
//!   bench group on both sides (`benches/common/mod.rs`), as
//!   `canary/<bench>.<group>.before` and `.after` ([`group_canary_id`]).
//!   They are the **nearest canaries** of the benchmarks in that group
//!   ([`Canaries`]). If one of them moved, **the attempt is noisy for those
//!   benchmarks** ([`Row::noisy`]). A group with no canary that ran on both
//!   sides (a base from before group canaries) is judged by the run-wide
//!   canaries instead.
//!
//! The group canaries are there because the run-wide ones can be minutes
//! away from the benchmark they vouch for. Run 37069372104 failed on
//! `marks/next_wide` at +32% on both attempts, in code that was identical
//! instruction for instruction on both sides (a local A/B put it within
//! 0.4%). The noise was VM contention that lasted minutes and started part
//! way through a run, between the canaries at its start and end. On CI,
//! `marks/next_wide` swings from −20% to +28% on unrelated commits.
//!
//! # Up to three attempts
//!
//! `just bench-compare` runs up to three attempts, and **a regression fails
//! only if it regressed on every attempt**. [`Report::outcome`] turns each
//! attempt's report into what to do next ([`Outcome`]).
//!
//! 1. **Attempt 1** benchmarks the base commit first, then this checkout
//!    (head). It is rerun if the run is noisy or a benchmark regressed,
//!    noisy for it or not.
//! 2. **Attempt 2, the rerun,** benchmarks head first, then base, and is
//!    judged with attempt 1 ([`Report::after`]). A benchmark that regressed
//!    on both attempts, both quiet for it, **fails**
//!    ([`Status::Regression`]). If either attempt was noisy for it, it is
//!    [`Status::Recheck`], and goes to a third attempt.
//! 3. **Attempt 3** runs only the bench groups with a benchmark to
//!    recheck ([`Report::third_attempt_plan`]), base first, and settles
//!    them ([`Report::settle`]). A benchmark that regressed on attempt 3
//!    too, noisy for it or quiet, **fails**. One that didn't **passes with
//!    a warning** ([`Status::Cleared`]), unless attempt 3 was noisy for it
//!    in the direction that can hide a regression: then it is
//!    **inconclusive** ([`Status::Unsettled`]).
//!
//! **Which way the noise went matters when a regression doesn't repeat.**
//! A change is head's relative to base, so noise can fake a regression
//! only by slowing head, and hide one only by slowing base. A canary that
//! got faster on head relative to base by more than the noise threshold
//! (base slowed) means the attempt may have hidden a regression
//! ([`Row::could_hide`]); one that got slower can't hide one. So a
//! regression that a later attempt doesn't show passes with a warning
//! ([`Report::unconfirmed`] after the rerun, [`Status::Cleared`] after a
//! third attempt) unless that attempt's nearest canaries say its base side
//! slowed: then it is [`Status::Unsettled`], the same on the rerun as on
//! attempt 3.
//!
//! A benchmark that regressed only on the rerun isn't judged
//! ([`Status::UnconfirmedRegression`]): the run is inconclusive. A rerun
//! whose run was noisy is inconclusive too. **Inconclusive warns and
//! passes**: failing the job because the runner was noisy would turn CI
//! red for no reason. Nothing inconclusive passes silently: `bench-report`
//! warns for each case.
//!
//! What still fails, and what noise can still do: a regression that shows
//! on every attempt fails, at attempt 2 if both were quiet for it, and at
//! attempt 3 otherwise, noisy or not, so a real regression still fails.
//! But noise between one group's two canaries, which neither canary sees,
//! looks quiet: if it slows head in the same group on both attempts, the
//! benchmark fails at attempt 2, with no third attempt. The group canaries
//! narrow that gap from a whole run to one group; they don't close it.
//! Budgets don't change: they fail on any attempt. On main at `ed09773`, attempt
//! 1 showed no regression, and the rerun showed 13 in code that was
//! identical on both sides, because noise began part way through it
//! (`docs/tasks/1.2b.md`).
//!
//! # The attempts alternate the order of the sides
//!
//! Attempts 1 and 3 benchmark base first, and the rerun head first
//! ([`Order`]). A steady drift over the job (the runner warming up, or a
//! neighbour's load building) counts against whichever side runs second,
//! so it counts against head on one attempt and for it on the other, and
//! can't make a regression show on both. On main at `2cc71bb`, both
//! attempts ran base first, and both showed `rows/parse` at +26%, which a
//! run on a quiet Mac put at −3.6% (`docs/tasks/1.2b.md`).
//! `bench-compare` records each attempt's order in its criterion
//! directory ([`ORDER_FILE`], read by [`read_order`]), and the report says
//! which order each attempt ran in.
//!
//! Criterion compares the side that runs second with the one that ran
//! first. So on a head-first attempt, criterion's `change/` is base's
//! change relative to head, and [`collect`] inverts it
//! ([`Change::inverted`]): every change in a report is head's, relative to
//! base.
//!
//! `sequential_read`, the other baseline benchmark, is reported for
//! information only ([`Status::Info`]): it is no canary, has no budget and
//! can't regress. It takes about 7 ms on CI and depends on the page cache
//! and the VM's I/O, so on a shared GitHub runner it moved by 10–16%
//! between runs of identical code while `memchr3_scan` (CPU-bound, about
//! 47 ms) stayed within 6% (`docs/tasks/1.2b.md`).

use std::fmt::Write as _;
use std::fs;
use std::io;
use std::path::Path;

use serde_json::Value;

use crate::budgets::Budget;

/// The criterion groups of the speed-of-light baseline: `baseline` runs
/// with the other benchmarks, and `bench-compare` runs it again as
/// `baseline-late` after them (`benches/baseline.rs`).
pub const BASELINE_GROUPS: [&str; 2] = ["baseline", "baseline-late"];

/// The run-wide noise canaries: the CPU-bound scan, in both baseline
/// groups. The other baseline benchmark, `sequential_read`, is too short
/// and too sensitive to I/O to be one (see the module docs). The group
/// canaries are [`GROUP_CANARY`]'s.
pub const CANARIES: [&str; 2] = ["baseline/memchr3_scan", "baseline-late/memchr3_scan"];

/// True if `id` (`group/name`) is a run-wide noise canary.
#[must_use]
pub fn is_canary(id: &str) -> bool {
    CANARIES.contains(&id)
}

/// The criterion group of the group canaries,
/// `canary/<bench>.<group>.<side>` ([`group_canary_id`]).
/// `benches/common/mod.rs` runs one just before and one just after each
/// bench group (see the module docs).
pub const GROUP_CANARY: &str = "canary";

/// Which side of its bench group a group canary runs on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    /// Just before the group's first benchmark.
    Before,
    /// Just after the group's last benchmark.
    After,
}

impl Side {
    /// The side's name in a canary's id: `before` or `after`.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Before => "before",
            Self::After => "after",
        }
    }
}

/// The id of `group`'s canary on `side`, in the bench target `bench`:
/// `canary/index.marks.before` for the canary before the `marks` group in
/// `benches/index.rs`. The bench target is in the id so that a third
/// attempt knows which benchmark binary to run
/// ([`Report::third_attempt_plan`]).
#[must_use]
pub fn group_canary_id(bench: &str, group: &str, side: Side) -> String {
    format!("{GROUP_CANARY}/{}", group_canary_name(bench, group, side))
}

/// The canary's name within the [`GROUP_CANARY`] group, which criterion
/// puts after the group's: `index.marks.before` ([`group_canary_id`]).
#[must_use]
pub fn group_canary_name(bench: &str, group: &str, side: Side) -> String {
    format!("{bench}.{group}.{}", side.name())
}

/// A group canary's id, taken apart ([`GroupCanary::parse`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GroupCanary<'a> {
    /// The bench target it ran in: `index`.
    pub bench: &'a str,
    /// The bench group it runs next to: `marks`.
    pub group: &'a str,
    /// Which side of the group.
    pub side: Side,
}

impl<'a> GroupCanary<'a> {
    /// `id` taken apart, if it is a group canary's ([`group_canary_id`]).
    #[must_use]
    pub fn parse(id: &'a str) -> Option<Self> {
        let rest = id.strip_prefix(GROUP_CANARY)?.strip_prefix('/')?;
        let (rest, side) = rest.rsplit_once('.')?;
        let side = [Side::Before, Side::After]
            .into_iter()
            .find(|known| known.name() == side)?;
        let (bench, group) = rest.split_once('.')?;
        (!bench.is_empty() && !group.is_empty()).then_some(Self { bench, group, side })
    }
}

/// True if `id` is a group canary's ([`group_canary_id`]).
#[must_use]
pub fn is_group_canary(id: &str) -> bool {
    GroupCanary::parse(id).is_some()
}

/// The bench group of `id` (`group/name`): `marks` for `marks/next_wide`.
#[must_use]
pub fn group_of(id: &str) -> &str {
    id.split_once('/').map_or(id, |(group, _)| group)
}

/// Benchmarks gated on their budget only, never on change between commits,
/// because shared runners make them too noisy to compare: the simulated
/// removable open reads through `pread` under background load, and its
/// 95% interval spans tens of percent between identical commits. The
/// simulated network share's open (task 2.0) reads the same way. A save
/// (task 2.2) writes the whole reference file and flushes it to the disk
/// (`F_FULLFSYNC`), so its time is the runner's disk's; so does Save As
/// UTF-8 (task 2.3).
pub const BUDGET_ONLY: [&str; 6] = [
    "open/first_paint_removable_under_load",
    "open/first_paint_share_under_load",
    "open/first_paint_slow_share_small",
    "open/first_paint_slow_share",
    "save/one_edit",
    "save/utf8_from_utf16",
];

/// True if `id` is gated on its budget only (see [`BUDGET_ONLY`]).
#[must_use]
pub fn is_budget_only(id: &str) -> bool {
    BUDGET_ONLY.contains(&id)
}

/// True if `id` is in one of the [`BASELINE_GROUPS`]. The baseline's code
/// is the same on both sides of every comparison, so its benchmarks never
/// gate: each is either a canary or information.
#[must_use]
pub fn is_baseline(id: &str) -> bool {
    id.split_once('/')
        .is_some_and(|(group, _)| BASELINE_GROUPS.contains(&group))
}

/// Which side `just bench-compare` benchmarked first on an attempt.
///
/// Attempt 1 runs [`Order::BaseFirst`] and the rerun [`Order::HeadFirst`],
/// so a drift over the job counts against head on one attempt and against
/// base on the other (see the module docs). Criterion compares the second
/// side with the first, so on a head-first attempt [`collect`] inverts its
/// changes, and either way a report's change is head's, relative to base.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Order {
    /// The base commit's benchmarks, then this checkout's.
    BaseFirst,
    /// This checkout's benchmarks, then the base commit's.
    HeadFirst,
}

/// The file in an attempt's criterion directory that records its
/// [`Order`]: `base-first` or `head-first`. `bench-compare` writes it.
pub const ORDER_FILE: &str = "bench-compare-order";

impl Order {
    /// The order's name in [`ORDER_FILE`].
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::BaseFirst => "base-first",
            Self::HeadFirst => "head-first",
        }
    }

    /// The order in a few words, for annotations: "base first".
    #[must_use]
    pub fn short(self) -> &'static str {
        match self {
            Self::BaseFirst => "base first",
            Self::HeadFirst => "head first",
        }
    }

    /// The order as a sentence's object, for the report.
    fn describe(self) -> &'static str {
        match self {
            Self::BaseFirst => "the base commit first, then this commit",
            Self::HeadFirst => "this commit first, then the base commit",
        }
    }
}

impl std::str::FromStr for Order {
    type Err = String;

    fn from_str(name: &str) -> Result<Self, Self::Err> {
        [Self::BaseFirst, Self::HeadFirst]
            .into_iter()
            .find(|order| order.name() == name)
            .ok_or_else(|| format!("`{name}` is not `base-first` or `head-first`"))
    }
}

/// Reads the [`Order`] that `bench-compare` recorded in `dir`
/// ([`ORDER_FILE`]), or `None` if it recorded none (a base with no
/// benchmarks, so nothing was compared).
///
/// # Errors
///
/// The file exists but can't be read, or doesn't name an order.
pub fn read_order(dir: &Path) -> io::Result<Option<Order>> {
    let file = dir.join(ORDER_FILE);
    match fs::read_to_string(&file) {
        Ok(text) => text
            .trim()
            .parse()
            .map(Some)
            .map_err(|why: String| malformed(&file, &why)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// One benchmark's result.
#[derive(Debug, Clone, PartialEq)]
pub struct Measurement {
    /// Criterion's id, `group/name`.
    pub id: String,
    /// The median time per iteration, in nanoseconds.
    pub median_ns: f64,
    /// Bytes processed per iteration, if the benchmark declares it.
    pub throughput_bytes: Option<u64>,
    /// The change from the baseline, if there was one to compare with.
    pub change: Option<Change>,
}

/// The relative change of a benchmark's median from the baseline, as
/// fractions: `0.1` is 10% slower, `-0.1` 10% faster.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Change {
    /// Criterion's point estimate.
    pub median: f64,
    /// The lower end of criterion's 95% confidence interval.
    pub lower: f64,
    /// The upper end of criterion's 95% confidence interval.
    pub upper: f64,
}

impl Change {
    /// The change the other way round: if this is A's change relative to
    /// B, the result is B's relative to A. A median ratio of `1 + c`
    /// becomes `1 / (1 + c)`, so +25% becomes −20%. The interval's ends
    /// swap, because the larger change becomes the smaller one. Inverting
    /// only ever reverses the order of two changes, so the inverted ends
    /// are still criterion's 95% interval: criterion takes them as percentiles of its bootstrap
    /// distribution, and inverting every resample keeps their order.
    #[must_use]
    pub fn inverted(self) -> Self {
        let invert = |change: f64| 1.0 / (1.0 + change) - 1.0;
        Self {
            median: invert(self.median),
            lower: invert(self.upper),
            upper: invert(self.lower),
        }
    }
}

/// The limits the report checks changes against, as fractions.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Thresholds {
    /// A benchmark regressed if its change's lower bound is above this.
    pub regression: f64,
    /// A canary moved if its median changed by more than this, either way.
    pub noise: f64,
}

/// What the report says about one benchmark.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    /// Within its budget and not a regression.
    Ok,
    /// A noise canary (run-wide or a group's) that held still.
    Canary,
    /// A noise canary that moved by more than the noise threshold: a
    /// run-wide one makes the run noisy, a group's makes the attempt noisy
    /// for its group's benchmarks.
    NoisyCanary,
    /// A baseline benchmark that isn't a canary (`sequential_read`), or a
    /// budget-only one within its budget: reported, never gated on change.
    Info,
    /// Slower than the baseline past the threshold, on an attempt quiet
    /// for it. On a rerun ([`Report::after`]): on both attempts, both
    /// quiet for it. After a third attempt ([`Report::settle`]): on all
    /// three. On a first attempt it is rerun to confirm; later, it fails.
    Regression,
    /// As `Regression`, but on an attempt noisy for it, so it can't be
    /// judged alone.
    NoisyRegression,
    /// A regression on the rerun that the first attempt didn't show, so it
    /// isn't judged ([`Report::after`]).
    UnconfirmedRegression,
    /// A regression on both attempts, at least one of them noisy for it: a
    /// third attempt settles it ([`Report::settle`]). If no third attempt
    /// may run, it fails, as a regression on every attempt.
    Recheck,
    /// A regression on both attempts (at least one noisy for it) that a
    /// third attempt didn't show, with no sign that its base side slowed.
    /// Passes, with a warning.
    Cleared,
    /// A regression that a later attempt didn't show, on an attempt whose
    /// nearest canaries say its base side slowed, which can hide a
    /// regression ([`Row::could_hide`]): attempt 1's on the rerun
    /// ([`Report::after`]), or attempts 1 and 2's on a third attempt
    /// ([`Report::settle`]). Not judged: inconclusive.
    Unsettled,
    /// The median is over the benchmark's budget. Fails.
    OverBudget,
    /// A budgeted benchmark that has no result. Fails.
    Missing,
}

/// What a row's status says in the report's table.
fn label(row: &Row) -> &'static str {
    match row.status {
        Status::Ok => "ok",
        Status::Canary => "canary",
        Status::NoisyCanary if is_group_canary(&row.id) => "canary moved (noisy for its group)",
        Status::NoisyCanary => "**canary moved (noisy run)**",
        Status::Info => "info (not gated)",
        Status::Regression => "**regression**",
        Status::NoisyRegression => "regression? (noisy for it)",
        Status::UnconfirmedRegression => "regression? (not on attempt 1)",
        Status::Recheck => "regression? (noisy for it; attempt 3 decides)",
        Status::Cleared => "regression? (not on attempt 3)",
        Status::Unsettled => "regression? (not shown again, base side slowed)",
        Status::OverBudget => "**over budget**",
        Status::Missing => "**missing**",
    }
}

/// The report's overall result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Nothing failed, and the run was quiet enough to judge.
    Pass,
    /// A budget failed, or a regression on an attempt quiet for it.
    Fail,
    /// A run-wide canary moved, so regressions can't be judged.
    Noisy,
}

/// What `just bench-compare` does with a report, given whether it may
/// still run another attempt ([`Report::outcome`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Nothing failed, and nothing was left unjudged.
    Pass,
    /// A budget failed, or a benchmark regressed on every attempt.
    Fail,
    /// Another attempt is needed: after attempt 1, a rerun of everything
    /// (the run was noisy or a benchmark regressed); after the rerun, a
    /// third attempt of the groups to recheck
    /// ([`Report::third_attempt_plan`]).
    Rerun,
    /// Nothing failed, but something wasn't judged: warn, but pass. The
    /// last full attempt's run was noisy, a regression on an attempt noisy
    /// for it couldn't be rerun, a regression showed on the rerun but not
    /// on attempt 1, or a later attempt that didn't show a regression may
    /// have hidden it ([`Status::Unsettled`]).
    Inconclusive,
}

impl Outcome {
    /// `bench-report`'s exit status: 0 to pass (inconclusive included), 1
    /// to fail, 3 to ask for another attempt.
    #[must_use]
    pub fn exit_code(self) -> u8 {
        match self {
            Self::Pass | Self::Inconclusive => 0,
            Self::Fail => 1,
            Self::Rerun => 3,
        }
    }

    /// The outcome in a few words, for the job summary's heading.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::Fail => "fail",
            Self::Rerun => "rerunning to confirm",
            Self::Inconclusive => "inconclusive",
        }
    }
}

/// A benchmark's nearest canaries on one attempt (see the module docs).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Canaries {
    /// Not judged by canaries: a canary itself, a baseline benchmark, a
    /// budget-only one, or a budget with no result.
    NotJudged,
    /// Its group's canaries' median changes, just before and just after
    /// the group: `None` for one that didn't run on both sides. If a group
    /// runs in two bench targets, each side's larger move.
    Group {
        /// The canary before the group.
        before: Option<f64>,
        /// The canary after the group.
        after: Option<f64>,
    },
    /// Its group has no canary that ran on both sides (a base from before
    /// group canaries), so the run-wide canaries judge it.
    RunWide,
}

impl Canaries {
    /// True if these canaries make the attempt noisy for their benchmark:
    /// a group canary moved by more than `noise`, or, for
    /// [`Canaries::RunWide`], the run is noisy (`run_noisy`).
    #[must_use]
    pub fn noisy(self, noise: f64, run_noisy: bool) -> bool {
        match self {
            Self::NotJudged => false,
            Self::Group { before, after } => [before, after]
                .into_iter()
                .flatten()
                .any(|change| change.abs() > noise),
            Self::RunWide => run_noisy,
        }
    }

    /// True if these canaries say the attempt may have hidden a
    /// regression in their benchmark: a group canary got faster on head
    /// relative to base (its base side slowed) by more than `noise`, or,
    /// for [`Canaries::RunWide`], a run-wide one did (`run_could_hide`).
    /// A canary that got slower can fake a regression but not hide one.
    #[must_use]
    pub fn could_hide(self, noise: f64, run_could_hide: bool) -> bool {
        match self {
            Self::NotJudged => false,
            Self::Group { before, after } => [before, after]
                .into_iter()
                .flatten()
                .any(|change| change < -noise),
            Self::RunWide => run_could_hide,
        }
    }

    /// The canaries for the report's table: `+1.0% / -3.0%`.
    fn describe(self) -> String {
        let one = |change: Option<f64>| change.map_or_else(|| "—".to_owned(), percent);
        match self {
            Self::NotJudged => String::new(),
            Self::Group { before, after } => format!("{} / {}", one(before), one(after)),
            Self::RunWide => "run-wide".to_owned(),
        }
    }
}

/// One row of the report.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    /// The benchmark id.
    pub id: String,
    /// Its result, if it ran.
    pub measurement: Option<Measurement>,
    /// Its budget, if it has one.
    pub budget: Option<Budget>,
    /// The verdict.
    pub status: Status,
    /// Its nearest canaries on this attempt.
    pub canaries: Canaries,
    /// True if this attempt was noisy for it ([`Canaries::noisy`]).
    pub noisy: bool,
    /// True if this attempt's noise could have hidden a regression in it:
    /// its base side slowed ([`Canaries::could_hide`]).
    pub could_hide: bool,
}

/// The whole report.
#[derive(Debug, Clone, PartialEq)]
pub struct Report {
    /// One row per benchmark (and per budget with no result), sorted by id.
    pub rows: Vec<Row>,
    /// The thresholds used.
    pub thresholds: Thresholds,
    /// True if a run-wide canary ([`CANARIES`]) moved by more than the
    /// noise threshold.
    pub noisy: bool,
    /// Which attempt this report judges: 1 from [`evaluate`], 2 after
    /// [`Report::after`], 3 after [`Report::settle`].
    pub attempt: u8,
    /// For a rerun ([`Report::after`]): the benchmarks that regressed on
    /// the first attempt but not on this one, sorted by id.
    pub unconfirmed: Vec<String>,
    /// The order this attempt ran the sides in, if known
    /// ([`read_order`]). [`evaluate`] leaves it `None`.
    pub order: Option<Order>,
    /// For a rerun ([`Report::after`]): the first attempt's order.
    pub first_order: Option<Order>,
    /// After a third attempt ([`Report::settle`]): the benchmarks it
    /// rechecked, sorted by id. Their rows' statuses say how it settled
    /// them.
    pub rechecked: Vec<String>,
    /// After a third attempt ([`Report::settle`]): its own report.
    pub third: Option<Box<Report>>,
}

/// What a third attempt runs ([`Report::third_attempt_plan`]): the bench
/// groups with a benchmark to recheck, and how to select them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    /// The bench groups, sorted.
    pub groups: Vec<String>,
    /// The bench targets that run them (`cargo bench --bench <name>`),
    /// sorted. Empty means every bench target, because a group has no
    /// canary to say which target runs it.
    pub benches: Vec<String>,
    /// A criterion filter (a regex over benchmark ids) for those groups'
    /// benchmarks and their group canaries.
    pub filter: String,
}

impl Plan {
    /// The plan as `bench-compare` reads it: the bench targets on the
    /// first line, separated by spaces (none for every target), and the
    /// filter on the second.
    #[must_use]
    pub fn to_text(&self) -> String {
        format!("{}\n{}\n", self.benches.join(" "), self.filter)
    }
}

/// Reads every benchmark result under `dir`, sorted by id. If `dir`'s
/// [`ORDER_FILE`] says the attempt ran head first, criterion compared
/// base with head, so each change is inverted ([`Change::inverted`]) to be
/// head's, relative to base.
///
/// # Errors
///
/// `dir` can't be read, a result file is missing or malformed, or the
/// order file is malformed ([`read_order`]).
pub fn collect(dir: &Path) -> io::Result<Vec<Measurement>> {
    let mut found = Vec::new();
    visit(dir, &mut found)?;
    found.sort_by(|a, b| a.id.cmp(&b.id));
    if read_order(dir)? == Some(Order::HeadFirst) {
        for measurement in &mut found {
            measurement.change = measurement.change.map(Change::inverted);
        }
    }
    Ok(found)
}

/// Collects the benchmark in `dir`, if it is one, and searches its
/// subdirectories. A benchmark directory is one with `new/benchmark.json`.
fn visit(dir: &Path, found: &mut Vec<Measurement>) -> io::Result<()> {
    let new = dir.join("new");
    if new.join("benchmark.json").is_file() {
        found.push(read_measurement(dir)?);
        return Ok(());
    }
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            visit(&entry.path(), found)?;
        }
    }
    Ok(())
}

fn read_measurement(dir: &Path) -> io::Result<Measurement> {
    let benchmark_file = dir.join("new").join("benchmark.json");
    let estimates_file = dir.join("new").join("estimates.json");
    let change_file = dir.join("change").join("estimates.json");

    let benchmark = read_json(&benchmark_file)?;
    let estimates = read_json(&estimates_file)?;
    let change = if change_file.is_file() {
        let median = &read_json(&change_file)?["median"];
        let number = |value: &Value| {
            value
                .as_f64()
                .ok_or_else(|| malformed(&change_file, "no median estimate"))
        };
        Some(Change {
            median: number(&median["point_estimate"])?,
            lower: number(&median["confidence_interval"]["lower_bound"])?,
            upper: number(&median["confidence_interval"]["upper_bound"])?,
        })
    } else {
        None
    };
    let id = benchmark["full_id"]
        .as_str()
        .ok_or_else(|| malformed(&benchmark_file, "no `full_id`"))?;
    let median_ns = estimates["median"]["point_estimate"]
        .as_f64()
        .ok_or_else(|| malformed(&estimates_file, "no median estimate"))?;
    Ok(Measurement {
        id: id.to_owned(),
        median_ns,
        throughput_bytes: benchmark["throughput"]["Bytes"].as_u64(),
        change,
    })
}

fn read_json(path: &Path) -> io::Result<Value> {
    let text = fs::read_to_string(path)?;
    serde_json::from_str(&text).map_err(|error| malformed(path, &error.to_string()))
}

fn malformed(path: &Path, why: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("{}: {why}", path.display()),
    )
}

/// Checks `measurements` against `budgets`, and their changes from the
/// baseline against `thresholds`, judging each benchmark's noise by its
/// nearest canaries (see the module docs).
#[must_use]
pub fn evaluate(
    measurements: &[Measurement],
    budgets: &[Budget],
    thresholds: Thresholds,
) -> Report {
    let moved = |m: &Measurement| {
        m.change
            .is_some_and(|change| change.median.abs() > thresholds.noise)
    };
    let noisy = measurements.iter().any(|m| is_canary(&m.id) && moved(m));
    let run_could_hide = measurements.iter().any(|m| {
        is_canary(&m.id)
            && m.change
                .is_some_and(|change| change.median < -thresholds.noise)
    });

    // Each group's canaries, by the group's name.
    let canaries_of = |group: &str| {
        let (mut before, mut after) = (None, None);
        for m in measurements {
            let (Some(canary), Some(change)) = (GroupCanary::parse(&m.id), m.change) else {
                continue;
            };
            if canary.group != group {
                continue;
            }
            let slot: &mut Option<f64> = match canary.side {
                Side::Before => &mut before,
                Side::After => &mut after,
            };
            if slot.is_none_or(|kept| change.median.abs() > kept.abs()) {
                *slot = Some(change.median);
            }
        }
        if before.is_none() && after.is_none() {
            Canaries::RunWide
        } else {
            Canaries::Group { before, after }
        }
    };

    let mut rows: Vec<Row> = measurements
        .iter()
        .map(|m| {
            let budget = budgets.iter().find(|b| b.id == m.id).copied();
            let canary = is_canary(&m.id) || is_group_canary(&m.id);
            let gated = !canary && !is_baseline(&m.id) && !is_budget_only(&m.id);
            let canaries = if gated {
                canaries_of(group_of(&m.id))
            } else {
                Canaries::NotJudged
            };
            let row_noisy = canaries.noisy(thresholds.noise, noisy);
            let could_hide = canaries.could_hide(thresholds.noise, run_could_hide);
            let status = if budget.is_some_and(|b| m.median_ns > b.max_ms * 1e6) {
                Status::OverBudget
            } else if canary && moved(m) {
                Status::NoisyCanary
            } else if canary {
                Status::Canary
            } else if !gated {
                Status::Info
            } else if m
                .change
                .is_some_and(|change| change.lower > thresholds.regression)
            {
                if row_noisy {
                    Status::NoisyRegression
                } else {
                    Status::Regression
                }
            } else {
                Status::Ok
            };
            Row {
                id: m.id.clone(),
                measurement: Some(m.clone()),
                budget,
                status,
                canaries,
                noisy: row_noisy,
                could_hide,
            }
        })
        .collect();

    for budget in budgets {
        if !measurements.iter().any(|m| m.id == budget.id) {
            rows.push(Row {
                id: budget.id.to_owned(),
                measurement: None,
                budget: Some(*budget),
                status: Status::Missing,
                canaries: Canaries::NotJudged,
                noisy: false,
                could_hide: false,
            });
        }
    }
    rows.sort_by(|a, b| a.id.cmp(&b.id));

    Report {
        rows,
        thresholds,
        noisy,
        attempt: 1,
        unconfirmed: Vec::new(),
        order: None,
        first_order: None,
        rechecked: Vec::new(),
        third: None,
    }
}

/// `text` with the regex metacharacters escaped, for a criterion filter.
fn regex_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if "\\.+*?()|[]{}^$".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

impl Report {
    /// Fail if a budget failed or a benchmark regressed on an attempt quiet
    /// for it; otherwise noisy if a run-wide canary moved; otherwise pass.
    /// A budget failure is a failure even on a noisy run: budgets are
    /// absolute.
    #[must_use]
    pub fn verdict(&self) -> Verdict {
        if self.any(&[Status::OverBudget, Status::Missing, Status::Regression]) {
            Verdict::Fail
        } else if self.noisy {
            Verdict::Noisy
        } else {
            Verdict::Pass
        }
    }

    /// True if a row has one of the `wanted` statuses.
    fn any(&self, wanted: &[Status]) -> bool {
        self.rows.iter().any(|row| wanted.contains(&row.status))
    }

    /// `id`'s row, if it has one.
    #[must_use]
    pub fn row(&self, id: &str) -> Option<&Row> {
        self.rows.iter().find(|row| row.id == id)
    }

    /// True if `id`'s whole interval was above the regression threshold
    /// in this report, on an attempt quiet for it or noisy.
    #[must_use]
    pub fn regressed(&self, id: &str) -> bool {
        self.row(id).is_some_and(|row| {
            matches!(
                row.status,
                Status::Regression
                    | Status::NoisyRegression
                    | Status::UnconfirmedRegression
                    | Status::Recheck
                    | Status::Cleared
            )
        })
    }

    /// This report, a rerun's, judged with the `first` attempt's. A
    /// regression here that `first` showed too is a
    /// [`Status::Regression`] if both attempts were quiet for it, and a
    /// [`Status::Recheck`] for a third attempt if either was noisy for it.
    /// One that `first` didn't show is a [`Status::UnconfirmedRegression`].
    /// A regression in `first` that this attempt doesn't show is
    /// [`Status::Unsettled`] if this attempt may have hidden it
    /// ([`Row::could_hide`]), as on a third attempt ([`Report::settle`]);
    /// otherwise it is listed in [`Report::unconfirmed`], and passes with
    /// a warning.
    #[must_use]
    pub fn after(mut self, first: &Report) -> Report {
        for row in &mut self.rows {
            let compared = row.measurement.as_ref().is_some_and(|m| m.change.is_some());
            if !matches!(row.status, Status::Regression | Status::NoisyRegression) {
                if compared
                    && row.could_hide
                    && first.regressed(&row.id)
                    && row.status == Status::Ok
                {
                    row.status = Status::Unsettled;
                }
                continue;
            }
            row.status = match first.row(&row.id) {
                Some(earlier) if first.regressed(&row.id) => {
                    if row.noisy || earlier.noisy {
                        Status::Recheck
                    } else {
                        Status::Regression
                    }
                }
                _ => Status::UnconfirmedRegression,
            };
        }
        self.unconfirmed = first
            .rows
            .iter()
            .filter(|row| {
                first.regressed(&row.id)
                    && !self.regressed(&row.id)
                    && self
                        .row(&row.id)
                        .is_none_or(|this| this.status != Status::Unsettled)
            })
            .map(|row| row.id.clone())
            .collect();
        self.first_order = first.order;
        self.attempt = 2;
        self
    }

    /// For a rerun ([`Report::after`]) with a [`Status::Recheck`]: what
    /// the third attempt runs. That is every benchmark in the groups of
    /// the benchmarks to recheck, and those groups' canaries, in the bench
    /// targets their canaries name, or in every bench target if a group
    /// has no canary to name one (the bench files check, as `open.rs` and
    /// `find.rs` do, what ran before asserting on it, so a filtered run is
    /// safe). `None` if there is nothing to recheck.
    #[must_use]
    pub fn third_attempt_plan(&self) -> Option<Plan> {
        let mut groups: Vec<String> = self
            .rows
            .iter()
            .filter(|row| row.status == Status::Recheck)
            .map(|row| group_of(&row.id).to_owned())
            .collect();
        groups.sort();
        groups.dedup();
        if groups.is_empty() {
            return None;
        }
        let mut benches = Vec::new();
        let mut every_target = false;
        for group in &groups {
            let targets: Vec<String> = self
                .rows
                .iter()
                .filter_map(|row| GroupCanary::parse(&row.id))
                .filter(|canary| canary.group == group.as_str())
                .map(|canary| canary.bench.to_owned())
                .collect();
            every_target |= targets.is_empty();
            benches.extend(targets);
        }
        benches.sort();
        benches.dedup();
        if every_target {
            benches.clear();
        }
        let names: Vec<String> = groups.iter().map(|group| regex_escape(group)).collect();
        let names = names.join("|");
        let filter =
            format!("^(?:(?:{names})/|{GROUP_CANARY}/[^/]*\\.(?:{names})\\.(?:before|after)$)");
        Some(Plan {
            groups,
            benches,
            filter,
        })
    }

    /// This report, a rerun's judged with the first attempt's
    /// ([`Report::after`]), settled by a `third` attempt of the groups to
    /// recheck ([`Report::third_attempt_plan`]). Each [`Status::Recheck`]
    /// becomes:
    ///
    /// - [`Status::Regression`] if `third` shows the regression too, noisy
    ///   for it or quiet, or has no change for it (nothing settled it);
    /// - [`Status::Unsettled`] if `third` doesn't, but may have hidden it:
    ///   its nearest canaries say its base side slowed ([`Row::could_hide`]);
    /// - [`Status::Cleared`] if `third` doesn't, otherwise.
    ///
    /// A benchmark over its budget on `third` is [`Status::OverBudget`]:
    /// budgets fail on any attempt.
    #[must_use]
    pub fn settle(mut self, third: Report) -> Report {
        let mut rechecked = Vec::new();
        for row in &mut self.rows {
            if row.status != Status::Recheck {
                continue;
            }
            rechecked.push(row.id.clone());
            row.status = match third.row(&row.id) {
                Some(again) if again.status == Status::OverBudget => Status::OverBudget,
                Some(again)
                    if matches!(again.status, Status::Regression | Status::NoisyRegression) =>
                {
                    Status::Regression
                }
                Some(again)
                    if again
                        .measurement
                        .as_ref()
                        .is_some_and(|m| m.change.is_some()) =>
                {
                    if again.could_hide {
                        Status::Unsettled
                    } else {
                        Status::Cleared
                    }
                }
                _ => Status::Regression,
            };
        }
        for again in &third.rows {
            if again.status == Status::OverBudget {
                match self.rows.iter_mut().find(|row| row.id == again.id) {
                    Some(row) => row.status = Status::OverBudget,
                    None => self.rows.push(again.clone()),
                }
            }
        }
        self.rows.sort_by(|a, b| a.id.cmp(&b.id));
        self.rechecked = rechecked;
        self.third = Some(Box::new(third));
        self.attempt = 3;
        self
    }

    /// For a rerun: true if both attempts are known to have run the sides
    /// in the same order, so a drift over the job counted against the same
    /// side twice. `bench-compare` swaps the order on the rerun, so this
    /// means something is wrong with it.
    #[must_use]
    pub fn same_order(&self) -> bool {
        matches!((self.first_order, self.order), (Some(first), Some(this)) if first == this)
    }

    /// What to do with this report.
    ///
    /// - A budget failure fails, on any attempt, noisy or not.
    /// - On attempt 1, unless it is the `last_attempt`, a noisy run or a
    ///   regression (noisy for it or not) is rerun, so that a regression
    ///   fails only if every attempt shows it.
    /// - On the rerun ([`Report::after`]), a [`Status::Regression`] (on
    ///   both attempts, both quiet for it) fails. Otherwise a
    ///   [`Status::Recheck`] asks for a third attempt
    ///   ([`Report::third_attempt_plan`]), unless this is the
    ///   `last_attempt`: then it fails, as a regression on every attempt.
    /// - On the last attempt, a [`Status::Regression`] fails. A noisy run,
    ///   a regression on an attempt noisy for it, one on the rerun only,
    ///   or one that a later attempt didn't show but may have hidden
    ///   ([`Status::Unsettled`]) is inconclusive, and passes: a noisy
    ///   runner is no reason to fail the job.
    #[must_use]
    pub fn outcome(&self, last_attempt: bool) -> Outcome {
        if self.any(&[Status::OverBudget, Status::Missing]) {
            return Outcome::Fail;
        }
        if !last_attempt {
            match self.attempt {
                1 if self.noisy || self.any(&[Status::Regression, Status::NoisyRegression]) => {
                    return Outcome::Rerun;
                }
                1 => return Outcome::Pass,
                2 if !self.any(&[Status::Regression]) && self.any(&[Status::Recheck]) => {
                    return Outcome::Rerun;
                }
                _ => {}
            }
        }
        if self.any(&[Status::Regression, Status::Recheck]) {
            Outcome::Fail
        } else if self.noisy
            || self.any(&[
                Status::NoisyRegression,
                Status::UnconfirmedRegression,
                Status::Unsettled,
            ])
        {
            Outcome::Inconclusive
        } else {
            Outcome::Pass
        }
    }

    /// The rows worth an annotation: failures, what is left unjudged, and
    /// what made the run noisy. A group canary that moved isn't one on its
    /// own: a regression it made noisy is.
    pub fn problems(&self) -> impl Iterator<Item = &Row> {
        self.rows.iter().filter(|row| {
            !matches!(row.status, Status::Ok | Status::Canary | Status::Info)
                && !(row.status == Status::NoisyCanary && is_group_canary(&row.id))
        })
    }

    /// The report as Markdown, for the terminal and for GitHub's job
    /// summary: a table of every benchmark, what the rules are, and, after
    /// a third attempt, a table of what it ran.
    #[must_use]
    pub fn markdown(&self) -> String {
        let mut out = String::new();
        if self.third.is_some() {
            out.push_str(
                "**The rerun** (attempt 2), judged with attempt 1, with attempt 3's verdict on \
                 each benchmark it rechecked:\n\n",
            );
        }
        table(&mut out, self.rows.iter().map(|row| (row, label(row))));
        let _ = write!(
            out,
            "\nA benchmark regressed if its change's whole 95% interval is above {regression}. \
             An attempt is noisy for a benchmark if one of its nearest canaries (`{GROUP_CANARY}/…`, \
             run just before and after its group on both sides) moved by more than {noise}, or, \
             if its group has none, if the run was noisy: if `{early}` or `{late}` moved by more \
             than {noise}. A regression fails only if it regressed on every attempt: on attempts 1 \
             and 2 if both were quiet for it, and otherwise on a third attempt too, of its group \
             alone, base first. Budgets fail on any attempt. The other `baseline` benchmarks are \
             information only.",
            regression = percent(self.thresholds.regression),
            noise = format!("{:.1}%", self.thresholds.noise * 100.0),
            early = CANARIES[0],
            late = CANARIES[1],
        );
        if let Some(order) = self.order {
            let attempt = if self.third.is_some() {
                "The rerun"
            } else {
                "This attempt"
            };
            let _ = write!(out, " {attempt} benchmarked {}.", order.describe());
        }
        match self.first_order {
            Some(first) if self.same_order() => {
                let _ = write!(
                    out,
                    " **Attempt 1 benchmarked {} too**, so a drift over the job counted \
                     against the same side on both attempts.",
                    first.describe()
                );
            }
            Some(first) => {
                let _ = write!(
                    out,
                    " Attempt 1 benchmarked {}. A drift over the job counts against \
                     whichever side runs second, so it can't make a regression show in \
                     both orders.",
                    first.describe()
                );
            }
            None => {}
        }
        if self.noisy {
            out.push_str(
                " **This run was noisy** (a run-wide canary moved), so regressions in groups \
                 with no canaries of their own can't be judged on it.",
            );
        }
        if !self.unconfirmed.is_empty() {
            let _ = write!(
                out,
                " Regressed on attempt 1 but not on {}, so not failed: {}.",
                if self.third.is_some() {
                    "the rerun"
                } else {
                    "this attempt"
                },
                code_list(&self.unconfirmed)
            );
        }
        out.push('\n');
        if let Some(third) = &self.third {
            let mut groups: Vec<String> = self
                .rechecked
                .iter()
                .map(|id| group_of(id).to_owned())
                .collect();
            groups.sort();
            groups.dedup();
            let _ = write!(
                out,
                "\n**Attempt 3** reran {} {}, which regressed on attempts 1 and 2 with at \
                 least one of them noisy for it",
                if self.rechecked.len() == 1 {
                    "the benchmark"
                } else {
                    "the benchmarks"
                },
                code_list(&self.rechecked),
            );
            if let Some(order) = third.order {
                let _ = write!(out, ". It benchmarked {}", order.describe());
            }
            let _ = writeln!(
                out,
                ", for the {} {} only. A rechecked benchmark fails if attempt 3 shows the \
                 regression too, noisy for it or not.\n",
                if groups.len() == 1 { "group" } else { "groups" },
                code_list(&groups)
            );
            let shown = third.rows.iter().filter(|row| {
                self.rechecked.contains(&row.id)
                    || is_canary(&row.id)
                    || GroupCanary::parse(&row.id)
                        .is_some_and(|canary| groups.iter().any(|group| group == canary.group))
                    || row.status == Status::OverBudget
            });
            table(
                &mut out,
                shown.map(|row| {
                    // A rechecked benchmark shows how attempt 3 settled it.
                    let settled = self
                        .row(&row.id)
                        .filter(|_| self.rechecked.contains(&row.id));
                    (row, settled.map_or_else(|| label(row), label))
                }),
            );
        }
        out
    }
}

/// Writes a table of `rows`, each with its status's label, to `out`.
fn table<'a>(out: &mut String, rows: impl Iterator<Item = (&'a Row, &'static str)>) {
    out.push_str(
        "| Benchmark | Median | Throughput | Change (95% interval) | Nearest canaries | Budget | Status |\n",
    );
    out.push_str("|---|---|---|---|---|---|---|\n");
    for (row, status) in rows {
        let (median, throughput, change) = match &row.measurement {
            Some(m) => (
                duration(m.median_ns),
                m.throughput_bytes
                    .map(|bytes| gib_per_s(bytes, m.median_ns))
                    .unwrap_or_default(),
                m.change.map_or_else(
                    || "—".to_owned(),
                    |c| {
                        format!(
                            "{} ({} to {})",
                            percent(c.median),
                            percent(c.lower),
                            percent(c.upper)
                        )
                    },
                ),
            ),
            None => ("—".to_owned(), String::new(), "—".to_owned()),
        };
        let budget = row
            .budget
            .map(|b| format!("{} ({})", duration(b.max_ms * 1e6), b.source))
            .unwrap_or_default();
        let _ = writeln!(
            out,
            "| `{}` | {median} | {throughput} | {change} | {} | {budget} | {status} |",
            row.id,
            row.canaries.describe(),
        );
    }
}

/// Ids as a Markdown list: "`a`, `b`".
fn code_list(ids: &[String]) -> String {
    ids.iter()
        .map(|id| format!("`{id}`"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// A fraction as a signed percentage: `+1.2%`.
fn percent(fraction: f64) -> String {
    format!("{:+.1}%", fraction * 100.0)
}

/// A duration in nanoseconds, to three significant figures.
fn duration(ns: f64) -> String {
    let (value, unit) = if ns >= 1e9 {
        (ns / 1e9, "s")
    } else if ns >= 1e6 {
        (ns / 1e6, "ms")
    } else if ns >= 1e3 {
        (ns / 1e3, "µs")
    } else {
        (ns, "ns")
    };
    let decimals = if value >= 100.0 {
        0
    } else if value >= 10.0 {
        1
    } else {
        2
    };
    format!("{value:.decimals$} {unit}")
}

/// Throughput in GiB/s, as criterion prints it.
fn gib_per_s(bytes: u64, ns: f64) -> String {
    // Precision loss above 2^53 bytes doesn't matter here.
    #[allow(clippy::cast_precision_loss)]
    let bytes = bytes as f64;
    format!("{:.2} GiB/s", bytes / (ns / 1e9) / f64::from(1u32 << 30))
}
