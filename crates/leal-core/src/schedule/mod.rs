//! The work scheduler: who runs what, when (DESIGN §3.10).
//!
//! Opening a file starts several jobs, in a strict priority order. Lower
//! priority work must never delay higher priority work, and above all it
//! must never delay the first rows appearing or make scrolling stutter.
//!
//! | Priority | Work | Where it runs |
//! |---|---|---|
//! | **P0** | Open, detect from the first 64 KB, parse the first screen | The caller's thread, before any job starts ([`crate::document`]) |
//! | [`Priority::P1`] | The row index | Its own thread, one per index job |
//! | [`Priority::P2`] | The whole-file review, diagnostics details (1.5) | The background pool |
//! | [`Priority::P3`] | Filter and sort acceleration (none yet) | The background pool, after P2 |
//!
//! The rules it enforces (DESIGN §3.10):
//!
//! - **Separate pools** (rule 4). Each index job gets its own thread. P2
//!   and P3 jobs share one `rayon` pool of (performance cores − 1)
//!   threads, so the main thread and the indexer always have a core free
//!   (but at least [`MIN_BACKGROUND_THREADS`]).
//!   Queued P2 jobs always start before queued P3 jobs, and P3 jobs never
//!   take the pool's last thread, so a P2 job never waits for a P3 job to
//!   finish.
//! - **Background work yields to the user** (rule 3). The app calls
//!   [`Scheduler::note_user_input`] on each scroll or edit event (or
//!   [`Scheduler::set_interacting`] around a gesture). P2 and P3 jobs then
//!   pause at their next [`Job::checkpoint`] and resume once input has been
//!   idle for [`IDLE_AFTER_INPUT`] (about 250 ms). The index (P1) never
//!   pauses: scrolling needs its rows. Jobs check in at least every ~5 ms
//!   of work, so a pause takes effect quickly.
//! - **Cancellation is explicit** (ADR-0005 decision 6). Every job has a
//!   [`JobHandle`] whose [`cancel`](JobHandle::cancel) sets a flag the job
//!   checks at its chunk boundaries. A paused job is woken to see it.
//! - **Instrumented.** Each job, first paint and each pause is an interval
//!   reported to the [`Platform`], which on macOS turns them into
//!   `os_signpost` intervals for Instruments (the app's platform is in
//!   `leal-ffi`, the only place besides `source` allowed `unsafe`). The
//!   platform also sets each thread's quality of service.
//!
//! ```
//! use std::sync::Arc;
//! use leal_core::schedule::{Interval, Priority, Scheduler, SchedulerConfig};
//!
//! let scheduler = Scheduler::new(SchedulerConfig::default())?;
//! let job = scheduler.spawn(Priority::P2, Interval::Review, |job| {
//!     let mut sum = 0u64;
//!     for chunk in 0..100u64 {
//!         job.checkpoint()?; // pauses while the user scrolls; stops if cancelled
//!         sum += chunk;
//!     }
//!     Ok(sum)
//! });
//! assert_eq!(job.wait(), Ok(&4950));
//! # Ok::<(), leal_core::schedule::StartError>(())
//! ```

mod input;
mod job;
#[cfg(test)]
mod tests;

use std::collections::VecDeque;
use std::fmt;
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::Duration;

use input::Input;
pub use job::{Job, JobControl, JobError, JobHandle};
use job::{JobState, run_job};

/// The smallest background pool: two threads, so that a P3 job can run
/// and still leave a thread for P2 work ("P3 never takes the pool's last
/// thread" always holds).
///
/// DESIGN §3.10 rule 4 sizes the pool at performance cores − 1, which is 1
/// on a Mac with 2 performance cores, and on the fallback path with 4 or
/// fewer cores. One extra thread there doesn't take a core from the main
/// thread or the indexer: pool threads run at utility QoS, below the main
/// thread (user-interactive) and the indexer (user-initiated), so the
/// kernel runs them only on cores those leave free (or on efficiency
/// cores), and they pause entirely while the user interacts. A one-thread
/// pool would instead let a P3 job hold up P2 work for as long as it ran.
pub const MIN_BACKGROUND_THREADS: usize = 2;

/// How long input must have been idle before paused background work
/// resumes (DESIGN §3.10 rule 3).
pub const IDLE_AFTER_INPUT: Duration = Duration::from_millis(250);

/// How urgent a job is. First paint (P0) isn't a job: it runs on the
/// caller's thread before any job starts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Priority {
    /// The row index, and so everything scrolling needs. Runs on its own
    /// thread, straight after first paint, and never pauses.
    P1,
    /// Checks and details: the whole-file review, diagnostics details.
    /// Runs on the background pool, alongside or after P1, and pauses
    /// while the user is interacting.
    P2,
    /// Acceleration structures for filter and sort. Runs on the background
    /// pool after any queued P2 job, never on the pool's last free thread,
    /// and pauses while the user is interacting.
    P3,
}

/// The kinds of thread the scheduler starts, for their quality of service.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ThreadClass {
    /// An index thread (P1): user-initiated.
    Index,
    /// A background pool thread (P2 and P3): utility.
    Background,
}

/// What an interval of work was, for Instruments.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Interval {
    /// P0: from opening (or re-reading) a file to its first screen of rows.
    FirstPaint,
    /// Building the row index (P1).
    Index,
    /// The whole-file encoding and dialect review (P2).
    Review,
    /// Diagnostics details (P2, task 1.5).
    Diagnostics,
    /// Filter and sort acceleration (P3).
    Acceleration,
    /// A P2 or P3 job waiting for the user to stop interacting.
    Paused,
}

impl Interval {
    /// The interval's name, as Instruments shows it.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Interval::FirstPaint => "First paint",
            Interval::Index => "Index",
            Interval::Review => "Review",
            Interval::Diagnostics => "Diagnostics",
            Interval::Acceleration => "Acceleration",
            Interval::Paused => "Paused",
        }
    }
}

/// What the scheduler needs from the operating system. Every method has a
/// default that does nothing, which is what tests and the CLI use
/// ([`NoPlatform`]). The app's implementation, in `leal-ffi`, sets thread
/// QoS and emits `os_signpost` intervals.
pub trait Platform: Send + Sync {
    /// Called on each thread the scheduler starts, before it runs a job.
    fn thread_started(&self, class: ThreadClass) {
        let _ = class;
    }

    /// The number of performance cores, if the platform knows it.
    fn performance_cores(&self) -> Option<usize> {
        None
    }

    /// An interval of work began. `id` is unique among intervals of this
    /// scheduler and never 0; the same `id` ends it.
    fn begin(&self, interval: Interval, id: u64) {
        let _ = (interval, id);
    }

    /// The interval `id`, begun with [`begin`](Self::begin), ended.
    fn end(&self, interval: Interval, id: u64) {
        let _ = (interval, id);
    }
}

/// A [`Platform`] that does nothing.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoPlatform;

impl Platform for NoPlatform {}

/// How to set up a [`Scheduler`].
#[derive(Clone)]
pub struct SchedulerConfig {
    /// Thread QoS and instrumentation.
    pub platform: Arc<dyn Platform>,
    /// The background pool's size. `None` means performance cores − 1,
    /// from the platform, or from the number of cores if it doesn't know.
    /// Either way it is at least [`MIN_BACKGROUND_THREADS`].
    pub background_threads: Option<usize>,
    /// How long input must be idle before paused work resumes.
    pub idle_after_input: Duration,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        SchedulerConfig {
            platform: Arc::new(NoPlatform),
            background_threads: None,
            idle_after_input: IDLE_AFTER_INPUT,
        }
    }
}

impl fmt::Debug for SchedulerConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SchedulerConfig")
            .field("background_threads", &self.background_threads)
            .field("idle_after_input", &self.idle_after_input)
            .finish_non_exhaustive()
    }
}

/// The background pool couldn't be started.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StartError {
    message: String,
}

impl fmt::Display for StartError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "couldn't start the background threads: {}", self.message)
    }
}

impl std::error::Error for StartError {}

/// Runs jobs by priority. One scheduler serves the whole app: every
/// document's jobs share its pool, and the user's input pauses all their
/// background work. Cloning it gives another handle to the same scheduler.
#[derive(Clone)]
pub struct Scheduler {
    shared: Arc<Shared>,
}

/// What jobs and their handles share with the scheduler.
struct Shared {
    platform: Arc<dyn Platform>,
    input: Arc<Input>,
    pool: rayon::ThreadPool,
    background_threads: usize,
    queues: Mutex<Queues>,
    /// The next interval id. Ids start at 1: `os_signpost` reserves 0.
    next_id: std::sync::atomic::AtomicU64,
}

/// Jobs waiting for a pool thread.
#[derive(Default)]
struct Queues {
    p2: VecDeque<Task>,
    p3: VecDeque<Task>,
    /// P3 jobs running now.
    running_p3: usize,
}

/// A job, ready to run.
type Task = Box<dyn FnOnce() + Send>;

impl Scheduler {
    /// Starts the background pool.
    ///
    /// # Errors
    ///
    /// [`StartError`] if the pool's threads can't be started.
    pub fn new(config: SchedulerConfig) -> Result<Scheduler, StartError> {
        let platform = config.platform;
        let background_threads = config
            .background_threads
            .unwrap_or_else(|| default_background_threads(platform.performance_cores()))
            .max(MIN_BACKGROUND_THREADS);
        let start_platform = Arc::clone(&platform);
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(background_threads)
            .thread_name(|i| format!("leal-background-{i}"))
            .start_handler(move |_| start_platform.thread_started(ThreadClass::Background))
            .build()
            .map_err(|error| StartError {
                message: error.to_string(),
            })?;
        Ok(Scheduler {
            shared: Arc::new(Shared {
                platform,
                input: Arc::new(Input::new(config.idle_after_input)),
                pool,
                background_threads,
                queues: Mutex::new(Queues::default()),
                next_id: std::sync::atomic::AtomicU64::new(1),
            }),
        })
    }

    /// The number of threads in the background pool.
    #[must_use]
    pub fn background_threads(&self) -> usize {
        self.shared.background_threads
    }

    /// The user scrolled, typed or clicked. P2 and P3 jobs pause at their
    /// next checkpoint and resume once input has been idle for
    /// [`SchedulerConfig::idle_after_input`]. Cheap enough to call on every
    /// event, on the main thread: it takes a lock no one holds for long.
    pub fn note_user_input(&self) {
        self.shared.input.note();
    }

    /// The user is in the middle of a gesture (`true`), such as a scroll
    /// with momentum or a drag, or it has ended (`false`). Background work
    /// stays paused while it lasts, and resumes once input has then been
    /// idle for [`SchedulerConfig::idle_after_input`].
    pub fn set_interacting(&self, interacting: bool) {
        self.shared.input.set_interacting(interacting);
    }

    /// Whether background (P2 and P3) work is held for the user right now.
    #[must_use]
    pub fn is_holding_background_work(&self) -> bool {
        self.shared.input.should_wait()
    }

    /// Starts `work` at `priority`, as a job reported to the platform as
    /// `interval`. P1 work gets a new thread; P2 and P3 work is queued for
    /// the background pool.
    ///
    /// `work` is given its [`Job`]: it calls [`Job::checkpoint`] at least
    /// every few milliseconds, and returns early with the error that gives
    /// (the job was cancelled). A panic in `work` is caught and becomes
    /// [`JobError::Panicked`].
    pub fn spawn<T, F>(&self, priority: Priority, interval: Interval, work: F) -> JobHandle<T>
    where
        T: Send + Sync + 'static,
        F: FnOnce(&Job) -> Result<T, JobError> + Send + 'static,
    {
        let (handle, state) = self.new_job(priority, interval);
        self.start(priority, run_job(Arc::clone(&self.shared), state, work));
        handle
    }

    /// [`spawn`](Self::spawn), but only once `after` has finished, however
    /// it finished. Until then the job is waiting, not queued; cancelling
    /// it meanwhile means it never runs.
    ///
    /// For example, the review of a file on a removable drive needs the
    /// whole file mapped, which happens only when its index pass (and copy)
    /// is complete.
    pub fn spawn_after<T, F>(
        &self,
        after: &JobControl,
        priority: Priority,
        interval: Interval,
        work: F,
    ) -> JobHandle<T>
    where
        T: Send + Sync + 'static,
        F: FnOnce(&Job) -> Result<T, JobError> + Send + 'static,
    {
        let (handle, state) = self.new_job(priority, interval);
        let scheduler = self.clone();
        let task = run_job(Arc::clone(&self.shared), state, work);
        after.on_finish(move || scheduler.start(priority, task));
        handle
    }

    /// An interval that isn't a job, such as first paint: it begins now and
    /// ends when the guard is dropped.
    pub fn interval(&self, interval: Interval) -> IntervalGuard {
        IntervalGuard::begin(Arc::clone(&self.shared.platform), interval, self.next_id())
    }

    fn next_id(&self) -> u64 {
        self.shared.next_id()
    }

    fn new_job<T>(
        &self,
        priority: Priority,
        interval: Interval,
    ) -> (JobHandle<T>, Arc<JobState<T>>) {
        let state = Arc::new(JobState::new(
            self.next_id(),
            priority,
            interval,
            Arc::clone(&self.shared.input),
        ));
        (JobHandle::new(Arc::clone(&state)), state)
    }

    /// Runs `task` where `priority` says.
    fn start(&self, priority: Priority, task: job::Prepared) {
        match priority {
            Priority::P1 => {
                let platform = Arc::clone(&self.shared.platform);
                let control = task.control.clone();
                let spawned =
                    thread::Builder::new()
                        .name("leal-index".to_owned())
                        .spawn(move || {
                            platform.thread_started(ThreadClass::Index);
                            task.run();
                        });
                if let Err(error) = spawned {
                    control.finish(Err(JobError::Failed(format!(
                        "couldn't start a thread: {error}"
                    ))));
                }
            }
            Priority::P2 | Priority::P3 => {
                {
                    let mut queues = self.shared.lock_queues();
                    let queue = if priority == Priority::P2 {
                        &mut queues.p2
                    } else {
                        &mut queues.p3
                    };
                    queue.push_back(Box::new(move || task.run()));
                }
                Shared::dispatch_later(&self.shared);
            }
        }
    }
}

impl fmt::Debug for Scheduler {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Scheduler")
            .field("background_threads", &self.shared.background_threads)
            .finish_non_exhaustive()
    }
}

impl Shared {
    fn next_id(&self) -> u64 {
        self.next_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    }

    fn lock_queues(&self) -> std::sync::MutexGuard<'_, Queues> {
        // Nothing that can panic runs while the lock is held, so a
        // poisoned lock still holds consistent queues.
        self.queues.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The most P3 jobs that may run at once: all the pool's threads but
    /// one, so a P2 job never waits for a P3 job to finish.
    fn p3_limit(&self) -> usize {
        // The pool has at least 2 threads, so this is at least 1.
        self.background_threads - 1
    }

    /// Asks the pool to run the next queued job when it has a thread free.
    /// Every job queued is matched by one of these calls, so none is left
    /// behind; one that finds nothing to do just returns.
    fn dispatch_later(shared: &Arc<Shared>) {
        let me = Arc::clone(shared);
        shared.pool.spawn(move || Shared::dispatch(&me));
    }

    /// Runs the most urgent queued job, if there is one this thread may
    /// take.
    fn dispatch(shared: &Arc<Shared>) {
        let (task, is_p3) = {
            let mut queues = shared.lock_queues();
            if let Some(task) = queues.p2.pop_front() {
                (task, false)
            } else if queues.running_p3 < shared.p3_limit()
                && let Some(task) = queues.p3.pop_front()
            {
                queues.running_p3 += 1;
                (task, true)
            } else {
                return;
            }
        };
        task();
        if is_p3 {
            let more = {
                let mut queues = shared.lock_queues();
                queues.running_p3 -= 1;
                !queues.p3.is_empty()
            };
            // A P3 job skipped while this one ran can go now.
            if more {
                Shared::dispatch_later(shared);
            }
        }
    }
}

/// The default background pool size: performance cores − 1, so the main
/// thread and the indexer always have a core (DESIGN §3.10 rule 4). The
/// app's platform reads the number of performance cores from the system.
/// Without it (tests, the CLI), half the cores is the guess. Never fewer
/// than [`MIN_BACKGROUND_THREADS`].
fn default_background_threads(performance_cores: Option<usize>) -> usize {
    let cores = performance_cores
        .unwrap_or_else(|| thread::available_parallelism().map_or(2, |n| n.get().div_ceil(2)));
    cores.saturating_sub(1).max(MIN_BACKGROUND_THREADS)
}

/// An interval reported to the [`Platform`]: begun when made, ended when
/// dropped (DESIGN §3.10, "Measuring it").
#[must_use = "the interval ends when the guard is dropped"]
pub struct IntervalGuard {
    platform: Arc<dyn Platform>,
    interval: Interval,
    id: u64,
}

impl IntervalGuard {
    fn begin(platform: Arc<dyn Platform>, interval: Interval, id: u64) -> Self {
        platform.begin(interval, id);
        IntervalGuard {
            platform,
            interval,
            id,
        }
    }
}

impl Drop for IntervalGuard {
    fn drop(&mut self) {
        self.platform.end(self.interval, self.id);
    }
}

impl fmt::Debug for IntervalGuard {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IntervalGuard")
            .field("interval", &self.interval)
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}
