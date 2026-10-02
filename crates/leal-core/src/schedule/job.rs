//! Jobs: the handle the caller keeps ([`JobHandle`], [`JobControl`]), the
//! context the work is given ([`Job`]), and how a job runs.

use std::cell::Cell;
use std::fmt;
use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use super::input::Input;
use super::{Interval, IntervalGuard, Priority, Shared};
use crate::index::IndexError;
use crate::source::{ReadError, ReadErrorKind};

/// Why a job didn't produce its result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum JobError {
    /// It was cancelled ([`JobHandle::cancel`]).
    Cancelled,
    /// The file couldn't be read: its removable drive was disconnected, or
    /// it changed while it was being read without a snapshot (ADR-0006).
    Read(ReadErrorKind),
    /// Indexing failed (for example, the file is 4 GiB or more).
    Index(IndexError),
    /// The work panicked. The message is English, for logs.
    Panicked(String),
    /// Anything else, such as a thread that couldn't be started. English,
    /// for logs.
    Failed(String),
}

impl fmt::Display for JobError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            JobError::Cancelled => f.write_str("the job was cancelled"),
            JobError::Read(kind) => write!(f, "the file couldn't be read ({kind:?})"),
            JobError::Index(error) => error.fmt(f),
            JobError::Panicked(message) => write!(f, "the job panicked: {message}"),
            JobError::Failed(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for JobError {}

impl From<IndexError> for JobError {
    fn from(error: IndexError) -> Self {
        match error {
            IndexError::Cancelled => JobError::Cancelled,
            other => JobError::Index(other),
        }
    }
}

impl From<ReadError> for JobError {
    fn from(error: ReadError) -> Self {
        match error.kind() {
            ReadErrorKind::Cancelled => JobError::Cancelled,
            kind => JobError::Read(kind),
        }
    }
}

impl From<crate::detect::Cancelled> for JobError {
    fn from(_: crate::detect::Cancelled) -> Self {
        JobError::Cancelled
    }
}

/// The shared half of a job: its cancel flag and how it ended.
struct ControlInner {
    id: u64,
    priority: Priority,
    interval: Interval,
    cancel: AtomicBool,
    /// To wake the job if it is waiting for the user.
    input: Arc<Input>,
    outcome: Mutex<Outcome>,
    finished: Condvar,
    /// The longest stretch of work between two checkpoints, in nanoseconds.
    longest_chunk_ns: AtomicU64,
}

enum Outcome {
    /// Not finished: what to run when it is.
    Running(Vec<Box<dyn FnOnce() + Send>>),
    Finished(Result<(), JobError>),
}

/// Controls a job and tells how it ended, whatever its result type: the
/// part of a [`JobHandle`] that doesn't depend on the result. Cloning it
/// gives another handle to the same job.
#[derive(Clone)]
pub struct JobControl {
    inner: Arc<ControlInner>,
}

impl JobControl {
    /// The job's id: unique within its scheduler, and the id of its
    /// Instruments interval.
    #[must_use]
    pub fn id(&self) -> u64 {
        self.inner.id
    }

    /// The job's priority.
    #[must_use]
    pub fn priority(&self) -> Priority {
        self.inner.priority
    }

    /// What the job is, as Instruments shows it.
    #[must_use]
    pub fn interval(&self) -> Interval {
        self.inner.interval
    }

    /// Asks the job to stop. It stops at its next checkpoint, within one
    /// chunk of work (ADR-0005 decision 6); a job waiting for the user to
    /// stop interacting stops straight away, and one that hasn't started
    /// never runs. It then finishes with [`JobError::Cancelled`], unless it
    /// had already finished.
    pub fn cancel(&self) {
        self.inner.cancel.store(true, Ordering::Release);
        self.inner.input.wake();
    }

    /// Whether [`cancel`](Self::cancel) has been called.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.inner.cancel.load(Ordering::Acquire)
    }

    /// Whether the job has finished, however it finished.
    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.outcome().is_some()
    }

    /// How the job ended, or `None` while it is running (or waiting).
    #[must_use]
    pub fn outcome(&self) -> Option<Result<(), JobError>> {
        match &*self.lock() {
            Outcome::Running(_) => None,
            Outcome::Finished(outcome) => Some(outcome.clone()),
        }
    }

    /// Waits for the job to finish, and says how it ended. Don't call it on
    /// the main thread.
    ///
    /// # Errors
    ///
    /// The [`JobError`] the job ended with.
    pub fn wait(&self) -> Result<(), JobError> {
        let mut outcome = self.lock();
        loop {
            if let Outcome::Finished(result) = &*outcome {
                return result.clone();
            }
            outcome = self
                .inner
                .finished
                .wait(outcome)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }

    /// [`wait`](Self::wait), for at most `timeout`. `None` if the job
    /// hadn't finished by then.
    #[must_use]
    pub fn wait_timeout(&self, timeout: Duration) -> Option<Result<(), JobError>> {
        let deadline = Instant::now() + timeout;
        let mut outcome = self.lock();
        loop {
            if let Outcome::Finished(result) = &*outcome {
                return Some(result.clone());
            }
            let left = deadline.checked_duration_since(Instant::now())?;
            outcome = self
                .inner
                .finished
                .wait_timeout(outcome, left)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }

    /// Runs `then` once the job has finished: straight away, on this
    /// thread, if it already has; otherwise on the job's thread as it
    /// finishes. Keep it short: it delays whatever that thread does next.
    /// This is how an async function waits for a job without blocking a
    /// thread (`leal-ffi`), and how [`Scheduler::spawn_after`] queues a job
    /// behind another.
    ///
    /// [`Scheduler::spawn_after`]: super::Scheduler::spawn_after
    ///
    /// A panic in `then` is contained: it doesn't reach the thread that
    /// runs it (on the background pool, an uncaught panic would abort the
    /// app), and the other callbacks waiting for the job still run.
    pub fn on_finish(&self, then: impl FnOnce() + Send + 'static) {
        {
            let mut outcome = self.lock();
            if let Outcome::Running(waiting) = &mut *outcome {
                waiting.push(Box::new(then));
                return;
            }
        }
        contain(then);
    }

    /// The longest the job worked between two checkpoints so far, not
    /// counting time paused for the user. DESIGN §3.10 rule 3 asks for at
    /// most about 5 ms, so that pausing and cancelling take effect quickly.
    #[must_use]
    pub fn longest_chunk(&self) -> Duration {
        Duration::from_nanos(self.inner.longest_chunk_ns.load(Ordering::Relaxed))
    }

    /// Records how the job ended, and runs what was waiting for it.
    pub(super) fn finish(&self, result: Result<(), JobError>) {
        let waiting = {
            let mut outcome = self.lock();
            match std::mem::replace(&mut *outcome, Outcome::Finished(result)) {
                Outcome::Running(waiting) => waiting,
                // Finished twice can't happen; keep the first outcome.
                finished @ Outcome::Finished(_) => {
                    *outcome = finished;
                    Vec::new()
                }
            }
        };
        self.inner.finished.notify_all();
        // Outside the lock, so a callback may look at the job. Each on its
        // own: one that panics doesn't keep the rest from running.
        for then in waiting {
            contain(then);
        }
    }

    fn record_chunk(&self, took: Duration) {
        let nanos = u64::try_from(took.as_nanos()).unwrap_or(u64::MAX);
        self.inner
            .longest_chunk_ns
            .fetch_max(nanos, Ordering::Relaxed);
    }

    fn lock(&self) -> MutexGuard<'_, Outcome> {
        // Only plain stores happen under this lock (callbacks run outside
        // it), so a poisoned lock still holds a consistent outcome.
        self.inner
            .outcome
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

impl fmt::Debug for JobControl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("JobControl")
            .field("id", &self.inner.id)
            .field("priority", &self.inner.priority)
            .field("interval", &self.inner.interval)
            .field("cancelled", &self.is_cancelled())
            .field("outcome", &self.outcome())
            .finish()
    }
}

/// A job and, once it has finished, its result.
pub(super) struct JobState<T> {
    control: JobControl,
    value: OnceLock<T>,
}

impl<T> JobState<T> {
    pub(super) fn new(id: u64, priority: Priority, interval: Interval, input: Arc<Input>) -> Self {
        JobState {
            control: JobControl {
                inner: Arc::new(ControlInner {
                    id,
                    priority,
                    interval,
                    cancel: AtomicBool::new(false),
                    input,
                    outcome: Mutex::new(Outcome::Running(Vec::new())),
                    finished: Condvar::new(),
                    longest_chunk_ns: AtomicU64::new(0),
                }),
            },
            value: OnceLock::new(),
        }
    }
}

/// The handle to a job the caller keeps (ADR-0005 decision 6): cancel it,
/// wait for it, or read its result. Cloning it gives another handle to the
/// same job. Dropping every handle doesn't cancel the job.
pub struct JobHandle<T> {
    state: Arc<JobState<T>>,
}

impl<T> JobHandle<T> {
    pub(super) fn new(state: Arc<JobState<T>>) -> Self {
        JobHandle { state }
    }

    /// The part of the handle that doesn't depend on the result type.
    #[must_use]
    pub fn control(&self) -> &JobControl {
        &self.state.control
    }

    /// Asks the job to stop: see [`JobControl::cancel`].
    pub fn cancel(&self) {
        self.state.control.cancel();
    }

    /// Whether the job has finished, however it finished.
    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.state.control.is_finished()
    }

    /// Waits for the job to finish and returns its result. Don't call it
    /// on the main thread.
    ///
    /// # Errors
    ///
    /// The [`JobError`] the job ended with.
    pub fn wait(&self) -> Result<&T, JobError> {
        self.state.control.wait()?;
        self.value()
    }

    /// The job's result, or `None` while it is running.
    #[must_use]
    pub fn result(&self) -> Option<Result<&T, JobError>> {
        Some(self.state.control.outcome()?.and_then(|()| self.value()))
    }

    fn value(&self) -> Result<&T, JobError> {
        // A job that finished without an error has always set its value.
        self.state
            .value
            .get()
            .ok_or_else(|| JobError::Failed("the job left no result".to_owned()))
    }
}

impl<T> Clone for JobHandle<T> {
    fn clone(&self) -> Self {
        JobHandle {
            state: Arc::clone(&self.state),
        }
    }
}

impl<T> fmt::Debug for JobHandle<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("JobHandle")
            .field(&self.state.control)
            .finish()
    }
}

/// What a job's work is given: its cancel flag and checkpoints.
pub struct Job {
    control: JobControl,
    shared: Arc<Shared>,
    /// When the current stretch of work between checkpoints began. A `Cell`
    /// because only the job's own thread uses it.
    chunk_started: Cell<Instant>,
}

impl Job {
    /// The job's id.
    #[must_use]
    pub fn id(&self) -> u64 {
        self.control.id()
    }

    /// The job's priority.
    #[must_use]
    pub fn priority(&self) -> Priority {
        self.control.priority()
    }

    /// The job's cancel flag, for work that takes one, such as
    /// [`Indexer::run`](crate::index::Indexer::run) and
    /// [`Source::stream`](crate::source::Source::stream). They check it
    /// between chunks. Work that can pause should call
    /// [`checkpoint`](Self::checkpoint) instead.
    #[must_use]
    pub fn cancel_flag(&self) -> &AtomicBool {
        &self.control.inner.cancel
    }

    /// Whether the job has been cancelled.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.control.is_cancelled()
    }

    /// Call between chunks of work, at least every ~5 ms (DESIGN §3.10
    /// rule 3). For a P2 or P3 job, while the user is interacting, this
    /// waits until input has been idle long enough. It returns
    /// [`JobError::Cancelled`] if the job has been cancelled, so the work
    /// can return early with `?`.
    ///
    /// # Errors
    ///
    /// [`JobError::Cancelled`].
    pub fn checkpoint(&self) -> Result<(), JobError> {
        self.end_chunk();
        if self.is_cancelled() {
            return Err(JobError::Cancelled);
        }
        if self.priority() != Priority::P1 && self.shared.input.should_wait() {
            let paused = IntervalGuard::begin(
                Arc::clone(&self.shared.platform),
                Interval::Paused,
                self.shared.next_id(),
            );
            self.shared.input.wait_until_idle(self.cancel_flag());
            drop(paused);
            // Time spent waiting isn't work.
            self.chunk_started.set(Instant::now());
            if self.is_cancelled() {
                return Err(JobError::Cancelled);
            }
        }
        Ok(())
    }

    /// Ends the current stretch of work, recording how long it took.
    fn end_chunk(&self) {
        let now = Instant::now();
        self.control
            .record_chunk(now.saturating_duration_since(self.chunk_started.get()));
        self.chunk_started.set(now);
    }
}

impl fmt::Debug for Job {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Job").field(&self.control).finish()
    }
}

/// A job ready to start: its work, wrapped so that running it reports the
/// interval, catches a panic and records the outcome.
pub(super) struct Prepared {
    pub(super) control: JobControl,
    run: Box<dyn FnOnce() + Send>,
}

impl Prepared {
    pub(super) fn run(self) {
        (self.run)();
    }
}

/// Wraps `work` as the job `state`.
pub(super) fn run_job<T, F>(shared: Arc<Shared>, state: Arc<JobState<T>>, work: F) -> Prepared
where
    T: Send + Sync + 'static,
    F: FnOnce(&Job) -> Result<T, JobError> + Send + 'static,
{
    let control = state.control.clone();
    let run = move || {
        let control = state.control.clone();
        if control.is_cancelled() {
            control.finish(Err(JobError::Cancelled));
            return;
        }
        let interval = IntervalGuard::begin(
            Arc::clone(&shared.platform),
            control.interval(),
            control.id(),
        );
        let job = Job {
            control: control.clone(),
            shared,
            chunk_started: Cell::new(Instant::now()),
        };
        // `AssertUnwindSafe`: if `work` panics, nothing it was changing is
        // looked at again except through the outcome, which says it
        // panicked.
        let result = panic::catch_unwind(AssertUnwindSafe(|| work(&job)));
        job.end_chunk();
        drop(interval);
        let outcome = match result {
            Ok(Ok(value)) => {
                // Set once: this is the only place that sets it.
                let _ = state.value.set(value);
                Ok(())
            }
            Ok(Err(error)) => Err(error),
            Err(payload) => Err(JobError::Panicked(panic_message(payload.as_ref()))),
        };
        control.finish(outcome);
    };
    Prepared {
        control,
        run: Box::new(run),
    }
}

/// Runs `f`, containing a panic in it: the panic hook has already printed
/// it, and it goes no further. For code that runs on a scheduler thread
/// but isn't a job's work, such as [`JobControl::on_finish`]'s callbacks
/// and the [`Platform`](super::Platform)'s methods: on the background pool,
/// an uncaught panic would abort the app, and on an index thread it would
/// end the thread before the job's outcome is recorded (DESIGN §3.9: a
/// failure is a failed document, never a crash).
pub(super) fn contain(f: impl FnOnce()) {
    // `AssertUnwindSafe`: nothing `f` was changing is looked at again by
    // the code that called it.
    let _ = panic::catch_unwind(AssertUnwindSafe(f));
}

/// The text of a panic, if it had one.
fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(text) = payload.downcast_ref::<&str>() {
        (*text).to_owned()
    } else if let Some(text) = payload.downcast_ref::<String>() {
        text.clone()
    } else {
        "a panic without a message".to_owned()
    }
}
