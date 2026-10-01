//! Tests for the scheduler: cancellation within one chunk, pausing for the
//! user, priorities and pools, panics, and the platform hooks.
//!
//! Tests that need a job to be at a particular point hand-shake with it
//! over channels instead of sleeping, so they don't depend on timing.
//! Pausing is about time by nature (idle for 250 ms), so those tests stop
//! the scheduler's clock and move it themselves
//! ([`Input::use_manual_clock`]), and tell that a job is paused from its
//! `Paused` interval rather than from it going quiet for a while. A slow or
//! busy machine then can't let the idle time run out early or late.
//!
//! [`Input::use_manual_clock`]: super::input::Input::use_manual_clock

use super::*;

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::Instant;

/// Records every platform call.
#[derive(Default)]
struct Recorder {
    events: Mutex<Vec<Event>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Event {
    Thread(ThreadClass, String),
    Begin(Interval, u64),
    End(Interval, u64),
}

impl Recorder {
    fn events(&self) -> Vec<Event> {
        self.events.lock().unwrap().clone()
    }
}

impl Platform for Recorder {
    fn thread_started(&self, class: ThreadClass) {
        let name = thread::current().name().unwrap_or_default().to_owned();
        self.events.lock().unwrap().push(Event::Thread(class, name));
    }

    fn performance_cores(&self) -> Option<usize> {
        Some(4)
    }

    fn begin(&self, interval: Interval, id: u64) {
        self.events.lock().unwrap().push(Event::Begin(interval, id));
    }

    fn end(&self, interval: Interval, id: u64) {
        self.events.lock().unwrap().push(Event::End(interval, id));
    }
}

fn scheduler(threads: usize, idle: Duration) -> (Scheduler, Arc<Recorder>) {
    let recorder = Arc::new(Recorder::default());
    let scheduler = Scheduler::new(SchedulerConfig {
        platform: Arc::clone(&recorder) as Arc<dyn Platform>,
        background_threads: Some(threads),
        idle_after_input: idle,
    })
    .unwrap();
    (scheduler, recorder)
}

const LONG: Duration = Duration::from_secs(10);

/// A job that runs chunks until cancelled, telling the test as each chunk
/// starts and waiting for the go-ahead to finish it.
fn chunked_job(
    scheduler: &Scheduler,
    priority: Priority,
) -> (JobHandle<usize>, mpsc::Receiver<usize>, mpsc::Sender<()>) {
    let (started_tx, started) = mpsc::channel();
    let (go, go_rx) = mpsc::channel::<()>();
    let handle = scheduler.spawn(priority, Interval::Acceleration, move |job| {
        for chunk in 0.. {
            job.checkpoint()?;
            started_tx.send(chunk).unwrap();
            go_rx.recv().unwrap();
        }
        Ok(0)
    });
    (handle, started, go)
}

/// One blocking P2 job on every pool thread, so that jobs queued next have
/// to wait.
struct Blockers(Vec<(JobHandle<usize>, mpsc::Sender<()>)>);

impl Blockers {
    fn fill(scheduler: &Scheduler) -> Blockers {
        Blockers(
            (0..scheduler.background_threads())
                .map(|_| {
                    let (handle, started, go) = chunked_job(scheduler, Priority::P2);
                    assert_eq!(started.recv_timeout(LONG), Ok(0));
                    (handle, go)
                })
                .collect(),
        )
    }

    /// Frees every pool thread.
    fn release(self) {
        for (handle, go) in self.0 {
            handle.cancel();
            go.send(()).unwrap();
            assert_eq!(handle.wait(), Err(JobError::Cancelled));
        }
    }
}

// ---------------------------------------------------------------------------
// Cancellation (ADR-0005 decision 6)

#[test]
fn cancel_stops_a_running_job_within_one_chunk() {
    for priority in [Priority::P1, Priority::P2, Priority::P3] {
        let (scheduler, _) = scheduler(2, IDLE_AFTER_INPUT);
        let (handle, started, go) = chunked_job(&scheduler, priority);
        for chunk in 0..3 {
            assert_eq!(started.recv_timeout(LONG), Ok(chunk));
            go.send(()).unwrap();
        }
        // Cancel in the middle of chunk 3.
        assert_eq!(started.recv_timeout(LONG), Ok(3));
        handle.cancel();
        assert!(
            !handle.is_finished(),
            "the chunk in progress finishes first"
        );
        go.send(()).unwrap();
        assert_eq!(handle.wait(), Err(JobError::Cancelled));
        // No chunk 4 began.
        assert!(started.try_recv().is_err(), "{priority:?}");
        assert_eq!(handle.control().outcome(), Some(Err(JobError::Cancelled)));
    }
}

#[test]
fn a_job_cancelled_before_it_starts_never_runs() {
    let (scheduler, _) = scheduler(2, IDLE_AFTER_INPUT);
    let blockers = Blockers::fill(&scheduler);
    let ran = Arc::new(AtomicBool::new(false));
    let job = scheduler.spawn(Priority::P2, Interval::Review, {
        let ran = Arc::clone(&ran);
        move |_| {
            ran.store(true, Ordering::SeqCst);
            Ok(())
        }
    });
    job.cancel();
    blockers.release();
    assert_eq!(job.wait(), Err(JobError::Cancelled));
    assert!(!ran.load(Ordering::SeqCst));
}

#[test]
fn a_finished_job_keeps_its_result_after_a_cancel() {
    let (scheduler, _) = scheduler(2, IDLE_AFTER_INPUT);
    let job = scheduler.spawn(Priority::P2, Interval::Review, |_| Ok(42));
    assert_eq!(job.wait(), Ok(&42));
    job.cancel();
    assert_eq!(job.wait(), Ok(&42));
    assert_eq!(job.result(), Some(Ok(&42)));
}

// ---------------------------------------------------------------------------
// Pausing for the user (DESIGN §3.10 rule 3)

/// A job that counts its checkpoints until cancelled.
fn counting_job(scheduler: &Scheduler, priority: Priority) -> (JobHandle<()>, Arc<AtomicUsize>) {
    let count = Arc::new(AtomicUsize::new(0));
    let handle = scheduler.spawn(priority, Interval::Review, {
        let count = Arc::clone(&count);
        move |job| {
            loop {
                job.checkpoint()?;
                count.fetch_add(1, Ordering::SeqCst);
                thread::sleep(Duration::from_millis(1));
            }
        }
    });
    (handle, count)
}

/// Waits until `count` moves, or gives up after `timeout`.
fn moves_within(count: &AtomicUsize, timeout: Duration) -> bool {
    let from = count.load(Ordering::SeqCst);
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if count.load(Ordering::SeqCst) != from {
            return true;
        }
        thread::sleep(Duration::from_millis(1));
    }
    false
}

/// How many `Paused` intervals have begun and not ended: the jobs waiting
/// for the user right now. A job begins one just before it waits and ends
/// it just after, so a job counted here is not working.
fn paused_now(recorder: &Recorder) -> usize {
    let events = recorder.events();
    let begun = events
        .iter()
        .filter(|e| matches!(e, Event::Begin(Interval::Paused, _)))
        .count();
    let ended = events
        .iter()
        .filter(|e| matches!(e, Event::End(Interval::Paused, _)))
        .count();
    begun - ended
}

/// Waits until exactly `jobs` jobs are paused, or gives up after `timeout`.
fn paused_within(recorder: &Recorder, jobs: usize, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if paused_now(recorder) == jobs {
            return true;
        }
        thread::sleep(Duration::from_millis(1));
    }
    false
}

/// Gives paused jobs a moment in which they would resume if they were
/// wrongly going to, then says whether `jobs` jobs are still paused. Only a
/// wrong resume can fail it; a slow machine can only make it miss one.
fn still_paused(recorder: &Recorder, jobs: usize) -> bool {
    thread::sleep(Duration::from_millis(20));
    paused_now(recorder) == jobs
}

#[test]
fn background_jobs_pause_while_the_user_interacts_and_resume_when_idle() {
    let idle = Duration::from_millis(100);
    let tick = Duration::from_millis(1);
    let (scheduler, recorder) = scheduler(2, idle);
    // Time moves only when the test moves it, so each step below happens at
    // an exact time since the input, however slowly the machine runs it.
    let input = &scheduler.shared.input;
    input.use_manual_clock();
    let (p2, p2_count) = counting_job(&scheduler, Priority::P2);
    let (p3, p3_count) = counting_job(&scheduler, Priority::P3);
    let counts = || {
        (
            p2_count.load(Ordering::SeqCst),
            p3_count.load(Ordering::SeqCst),
        )
    };
    assert!(moves_within(&p2_count, LONG) && moves_within(&p3_count, LONG));
    assert_eq!(paused_now(&recorder), 0);

    scheduler.set_interacting(true);
    assert!(scheduler.is_holding_background_work());
    // Both jobs pause at their next checkpoint.
    assert!(paused_within(&recorder, 2, LONG));
    let paused_at = counts();
    // Still paused well past the idle time, because the gesture goes on.
    input.advance_clock(idle * 3);
    assert!(scheduler.is_holding_background_work());
    assert!(still_paused(&recorder, 2));
    assert_eq!(counts(), paused_at);

    // The gesture ends; work resumes once input has been idle for the idle
    // time, and not a moment before.
    scheduler.set_interacting(false);
    input.advance_clock(idle - tick);
    assert!(scheduler.is_holding_background_work());
    assert!(still_paused(&recorder, 2));
    assert_eq!(counts(), paused_at);
    input.advance_clock(tick);
    assert!(!scheduler.is_holding_background_work());
    assert!(paused_within(&recorder, 0, LONG));
    assert!(moves_within(&p2_count, LONG) && moves_within(&p3_count, LONG));

    // A single input event pauses work too, for the idle time, measured
    // from the event.
    scheduler.note_user_input();
    assert!(scheduler.is_holding_background_work());
    assert!(paused_within(&recorder, 2, LONG));
    let paused_at = counts();
    input.advance_clock(idle - tick);
    assert!(scheduler.is_holding_background_work());
    assert!(still_paused(&recorder, 2));
    assert_eq!(counts(), paused_at);
    input.advance_clock(tick);
    assert!(!scheduler.is_holding_background_work());
    assert!(paused_within(&recorder, 0, LONG));
    assert!(moves_within(&p2_count, LONG) && moves_within(&p3_count, LONG));

    p2.cancel();
    p3.cancel();
    assert_eq!(p2.wait(), Err(JobError::Cancelled));
    assert_eq!(p3.wait(), Err(JobError::Cancelled));
    // Each pause is one interval, begun and ended: one per job for each of
    // the two pauses above. Being woken while paused (by the gesture
    // ending, or the clock moving) doesn't begin another.
    let events = recorder.events();
    let begun = events
        .iter()
        .filter(|e| matches!(e, Event::Begin(Interval::Paused, _)))
        .count();
    assert_eq!((begun, paused_now(&recorder)), (4, 0), "{events:?}");
}

#[test]
fn the_index_never_pauses() {
    let (scheduler, recorder) = scheduler(2, Duration::from_secs(60));
    scheduler.set_interacting(true);
    let (p1, p1_count) = counting_job(&scheduler, Priority::P1);
    let (p2, p2_count) = counting_job(&scheduler, Priority::P2);
    assert!(moves_within(&p1_count, LONG));
    // P2 pauses at its first checkpoint, before it counts anything.
    assert!(paused_within(&recorder, 1, LONG));
    assert_eq!(p2_count.load(Ordering::SeqCst), 0, "P2 waits for the user");
    assert!(moves_within(&p1_count, LONG), "P1 keeps going");
    // The one paused job is P2: P1 never begins a pause.
    assert_eq!(paused_now(&recorder), 1);
    p1.cancel();
    p2.cancel();
    assert_eq!(p1.wait(), Err(JobError::Cancelled));
    assert_eq!(p2.wait(), Err(JobError::Cancelled));
}

#[test]
fn cancel_wakes_a_paused_job() {
    // During a gesture a paused job's own wait times out only after the
    // idle time, far longer than `LONG`: so only the cancel's wake-up can
    // end it in time.
    let idle = Duration::from_secs(60);
    assert!(idle > LONG * 2);
    let (scheduler, recorder) = scheduler(2, idle);
    let (job, count) = counting_job(&scheduler, Priority::P2);
    assert!(moves_within(&count, LONG));
    scheduler.set_interacting(true);
    assert!(paused_within(&recorder, 1, LONG));
    job.cancel();
    assert_eq!(
        job.control().wait_timeout(LONG),
        Some(Err(JobError::Cancelled))
    );
    assert_eq!(paused_now(&recorder), 0);
}

// ---------------------------------------------------------------------------
// Priorities and pools (DESIGN §3.10 rule 4)

#[test]
fn queued_p2_jobs_start_before_queued_p3_jobs() {
    let (scheduler, _) = scheduler(2, IDLE_AFTER_INPUT);
    let blockers = Blockers::fill(&scheduler);
    // Queue P3, P2, P3, P2 while every thread is busy. The P2 jobs hold
    // their thread until told to go on, and so do the P3 jobs.
    let (p3_a, p3_a_started, p3_a_go) = chunked_job(&scheduler, Priority::P3);
    let (p2_a, p2_a_started, p2_a_go) = chunked_job(&scheduler, Priority::P2);
    let (p3_b, p3_b_started, p3_b_go) = chunked_job(&scheduler, Priority::P3);
    let (p2_b, p2_b_started, p2_b_go) = chunked_job(&scheduler, Priority::P2);
    blockers.release();
    // Both freed threads take the P2 jobs, though the P3 jobs were queued
    // first; no P3 job starts while they hold the threads.
    assert_eq!(p2_a_started.recv_timeout(LONG), Ok(0));
    assert_eq!(p2_b_started.recv_timeout(LONG), Ok(0));
    assert!(
        p3_a_started
            .recv_timeout(Duration::from_millis(100))
            .is_err()
    );
    assert!(p3_b_started.try_recv().is_err());
    for (job, go) in [(&p2_a, &p2_a_go), (&p2_b, &p2_b_go)] {
        job.cancel();
        go.send(()).unwrap();
        assert_eq!(job.wait(), Err(JobError::Cancelled));
    }
    // Then the P3 jobs, in order, one at a time (the other thread is kept
    // for P2).
    assert_eq!(p3_a_started.recv_timeout(LONG), Ok(0));
    assert!(
        p3_b_started
            .recv_timeout(Duration::from_millis(100))
            .is_err()
    );
    p3_a.cancel();
    p3_a_go.send(()).unwrap();
    assert_eq!(p3_a.wait(), Err(JobError::Cancelled));
    assert_eq!(p3_b_started.recv_timeout(LONG), Ok(0));
    p3_b.cancel();
    p3_b_go.send(()).unwrap();
    assert_eq!(p3_b.wait(), Err(JobError::Cancelled));
}

#[test]
fn p3_jobs_leave_a_pool_thread_for_p2() {
    let (scheduler, _) = scheduler(2, IDLE_AFTER_INPUT);
    let (p3_a, started_a, go_a) = chunked_job(&scheduler, Priority::P3);
    assert_eq!(started_a.recv_timeout(LONG), Ok(0));
    // A second P3 job waits: it would take the last thread.
    let (p3_b, started_b, go_b) = chunked_job(&scheduler, Priority::P3);
    assert!(started_b.recv_timeout(Duration::from_millis(100)).is_err());
    // A P2 job gets the free thread straight away.
    let p2 = scheduler.spawn(Priority::P2, Interval::Review, |_| Ok("done"));
    assert_eq!(p2.control().wait_timeout(LONG), Some(Ok(())));
    // When the first P3 job ends, the second starts.
    p3_a.cancel();
    go_a.send(()).unwrap();
    assert_eq!(p3_a.wait(), Err(JobError::Cancelled));
    assert_eq!(started_b.recv_timeout(LONG), Ok(0));
    p3_b.cancel();
    go_b.send(()).unwrap();
    assert_eq!(p3_b.wait(), Err(JobError::Cancelled));
}

#[test]
fn each_index_job_gets_its_own_thread_and_the_pool_its_size() {
    let (scheduler, recorder) = scheduler(2, IDLE_AFTER_INPUT);
    assert_eq!(scheduler.background_threads(), 2);
    let names: Vec<_> = (0..3)
        .map(|_| {
            scheduler.spawn(Priority::P1, Interval::Index, |_| {
                Ok(thread::current().name().unwrap_or_default().to_owned())
            })
        })
        .collect();
    for name in &names {
        assert_eq!(name.wait().map(String::as_str), Ok("leal-index"));
    }
    let background = scheduler.spawn(Priority::P2, Interval::Review, |_| {
        Ok(thread::current().name().unwrap_or_default().to_owned())
    });
    assert!(background.wait().unwrap().starts_with("leal-background-"));
    let events = recorder.events();
    let index_threads = events
        .iter()
        .filter(|e| matches!(e, Event::Thread(ThreadClass::Index, name) if name == "leal-index"))
        .count();
    assert_eq!(index_threads, 3, "{events:?}");
    let pool_threads = events
        .iter()
        .filter(|e| matches!(e, Event::Thread(ThreadClass::Background, _)))
        .count();
    assert!((1..=2).contains(&pool_threads), "{events:?}");
}

#[test]
fn the_default_pool_is_performance_cores_minus_one() {
    assert_eq!(default_background_threads(Some(4)), 3);
    assert_eq!(default_background_threads(Some(8)), 7);
    // Never fewer than two, so P3 always leaves a thread for P2.
    assert_eq!(default_background_threads(Some(3)), 2);
    assert_eq!(default_background_threads(Some(2)), 2);
    assert_eq!(default_background_threads(Some(1)), 2);
    assert_eq!(default_background_threads(Some(0)), 2);
    assert!(default_background_threads(None) >= MIN_BACKGROUND_THREADS);
    // The recorder says 4 performance cores.
    let recorder = Arc::new(Recorder::default());
    let scheduler = Scheduler::new(SchedulerConfig {
        platform: recorder,
        ..SchedulerConfig::default()
    })
    .unwrap();
    assert_eq!(scheduler.background_threads(), 3);
    // Asked for one thread, it still has two.
    let (scheduler, _) = scheduler_of(1);
    assert_eq!(scheduler.background_threads(), 2);
}

fn scheduler_of(threads: usize) -> (Scheduler, Arc<Recorder>) {
    scheduler(threads, IDLE_AFTER_INPUT)
}

/// On the smallest pool, a long P3 job still leaves a thread for P2.
#[test]
fn the_smallest_pool_keeps_a_thread_for_p2() {
    let (scheduler, _) = scheduler_of(1);
    let (p3, started, go) = chunked_job(&scheduler, Priority::P3);
    assert_eq!(started.recv_timeout(LONG), Ok(0));
    let p2 = scheduler.spawn(Priority::P2, Interval::Review, |_| Ok(()));
    assert_eq!(p2.control().wait_timeout(LONG), Some(Ok(())));
    p3.cancel();
    go.send(()).unwrap();
    assert_eq!(p3.wait(), Err(JobError::Cancelled));
}

// ---------------------------------------------------------------------------
// Results, panics and waiting

#[test]
fn a_panic_becomes_an_error_and_the_pool_carries_on() {
    let (scheduler, _) = scheduler(2, IDLE_AFTER_INPUT);
    let job: JobHandle<()> = scheduler.spawn(Priority::P2, Interval::Review, |_| {
        panic!("deliberate");
    });
    assert_eq!(job.wait(), Err(JobError::Panicked("deliberate".to_owned())));
    let index: JobHandle<()> = scheduler.spawn(Priority::P1, Interval::Index, |_| {
        panic!("{}", String::from("on the index thread"));
    });
    assert_eq!(
        index.wait(),
        Err(JobError::Panicked("on the index thread".to_owned()))
    );
    let after = scheduler.spawn(Priority::P2, Interval::Review, |_| Ok(1));
    assert_eq!(after.wait(), Ok(&1));
}

#[test]
fn errors_from_the_work_are_the_outcome() {
    let (scheduler, _) = scheduler(2, IDLE_AFTER_INPUT);
    let job: JobHandle<()> = scheduler.spawn(Priority::P2, Interval::Review, |_| {
        Err(JobError::Read(ReadErrorKind::Disconnected))
    });
    assert_eq!(job.wait(), Err(JobError::Read(ReadErrorKind::Disconnected)));
    assert_eq!(
        JobError::from(IndexError::Cancelled),
        JobError::Cancelled,
        "a cancelled index is a cancelled job"
    );
    assert_eq!(
        JobError::from(IndexError::TooLarge { len: 1 }),
        JobError::Index(IndexError::TooLarge { len: 1 })
    );
}

use crate::index::IndexError;
use crate::source::ReadErrorKind;

#[test]
fn on_finish_runs_once_whenever_it_is_asked() {
    let (scheduler, _) = scheduler(2, IDLE_AFTER_INPUT);
    let (job, started, go) = chunked_job(&scheduler, Priority::P2);
    assert_eq!(started.recv_timeout(LONG), Ok(0));
    let calls = Arc::new(AtomicUsize::new(0));
    let count = || {
        let calls = Arc::clone(&calls);
        move || {
            calls.fetch_add(1, Ordering::SeqCst);
        }
    };
    job.control().on_finish(count());
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    job.cancel();
    go.send(()).unwrap();
    let _ = job.wait();
    // It ran on the job's thread before `wait` returned... or just after;
    // wait for it either way.
    let deadline = Instant::now() + LONG;
    while calls.load(Ordering::SeqCst) == 0 && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    // Asked after it finished: straight away.
    job.control().on_finish(count());
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[test]
fn spawn_after_waits_for_the_other_job() {
    let (scheduler, _) = scheduler(2, IDLE_AFTER_INPUT);
    let (first, started, go) = chunked_job(&scheduler, Priority::P1);
    assert_eq!(started.recv_timeout(LONG), Ok(0));
    let ran_after = Arc::new(AtomicBool::new(false));
    let second = scheduler.spawn_after(first.control(), Priority::P2, Interval::Review, {
        let first = first.control().clone();
        let ran_after = Arc::clone(&ran_after);
        move |_| {
            ran_after.store(first.is_finished(), Ordering::SeqCst);
            Ok(())
        }
    });
    assert!(
        second
            .control()
            .wait_timeout(Duration::from_millis(50))
            .is_none()
    );
    // The first job ends (here, cancelled); the second still runs.
    first.cancel();
    go.send(()).unwrap();
    assert_eq!(second.wait(), Ok(&()));
    assert!(ran_after.load(Ordering::SeqCst));

    // Cancelled while it waits: it never runs.
    let (first, started, go) = chunked_job(&scheduler, Priority::P1);
    assert_eq!(started.recv_timeout(LONG), Ok(0));
    let never = scheduler.spawn_after(first.control(), Priority::P2, Interval::Review, |_| {
        panic!("never runs");
    });
    never.cancel();
    first.cancel();
    go.send(()).unwrap();
    assert_eq!(never.wait(), Err::<&(), _>(JobError::Cancelled));
}

#[test]
fn the_longest_chunk_is_recorded_without_pauses() {
    let (scheduler, _) = scheduler(2, IDLE_AFTER_INPUT);
    let job = scheduler.spawn(Priority::P2, Interval::Review, |job| {
        for _ in 0..3 {
            job.checkpoint()?;
            thread::sleep(Duration::from_millis(20));
        }
        Ok(())
    });
    job.wait().unwrap();
    let longest = job.control().longest_chunk();
    assert!(longest >= Duration::from_millis(20), "{longest:?}");
    assert!(longest < Duration::from_secs(5), "{longest:?}");
}

// ---------------------------------------------------------------------------
// Instrumentation

#[test]
fn every_job_and_interval_is_reported_to_the_platform() {
    let (scheduler, recorder) = scheduler(2, IDLE_AFTER_INPUT);
    let first_paint = scheduler.interval(Interval::FirstPaint);
    drop(first_paint);
    let index = scheduler.spawn(Priority::P1, Interval::Index, |_| Ok(()));
    let review = scheduler.spawn(Priority::P2, Interval::Review, |_| Ok(()));
    index.wait().unwrap();
    review.wait().unwrap();
    let events: Vec<_> = recorder
        .events()
        .into_iter()
        .filter(|e| !matches!(e, Event::Thread(..)))
        .collect();
    for (interval, id) in [
        (Interval::Index, index.control().id()),
        (Interval::Review, review.control().id()),
    ] {
        let begin = events.iter().position(|e| *e == Event::Begin(interval, id));
        let end = events.iter().position(|e| *e == Event::End(interval, id));
        assert!(begin.is_some() && end > begin, "{interval:?}: {events:?}");
    }
    assert!(matches!(events[0], Event::Begin(Interval::FirstPaint, id) if id > 0));
    assert!(matches!(events[1], Event::End(Interval::FirstPaint, _)));
    // Ids are unique.
    let mut ids: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            Event::Begin(_, id) => Some(*id),
            _ => None,
        })
        .collect();
    let all = ids.len();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), all);
}

#[test]
fn a_job_cancelled_before_it_starts_reports_no_interval() {
    let (scheduler, recorder) = scheduler(2, IDLE_AFTER_INPUT);
    let blockers = Blockers::fill(&scheduler);
    let job = scheduler.spawn(Priority::P2, Interval::Review, |_| Ok(()));
    job.cancel();
    blockers.release();
    let _ = job.wait();
    let id = job.control().id();
    assert!(
        !recorder
            .events()
            .contains(&Event::Begin(Interval::Review, id))
    );
}

#[test]
fn handles_and_jobs_are_send_and_sync() {
    fn send_sync<T: Send + Sync>() {}
    send_sync::<Scheduler>();
    send_sync::<JobHandle<String>>();
    send_sync::<JobControl>();
    fn send<T: Send>() {}
    send::<Job>();
}
