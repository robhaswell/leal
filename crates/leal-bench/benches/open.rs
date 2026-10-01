//! First paint (PLAN 1.3a, DESIGN §1 and §3.10): from opening the
//! reference file to its first screen of rows, which DESIGN §1 budgets at
//! under 150 ms "independent of file size, before indexing finishes", with
//! P1–P3 work forced to run at the same time.
//!
//! The load, running for the whole measurement:
//!
//! - **P1**: a loop that opens the reference file as a document, waits for
//!   its index and opens it again, so an index thread is always running.
//!   Every measured open also starts its own index.
//! - **P2**: the review each of those opens starts (about half a second of
//!   work each), on the background pool.
//! - **P3**: one busy job per pool thread, reading the file in 256 KiB
//!   chunks until cancelled. P3 normally waits for P2; these are queued
//!   first, and nothing signals user input, so nothing pauses: the pool is
//!   kept full.
//!
//! `open/first_paint_under_load` opens the file from its internal volume
//! (clone and map). `open/first_paint_removable_under_load` opens it as if
//! it were on a removable drive (ADR-0006): first paint reads the start
//! with `pread`, and the index copies the file as it goes. That uses
//! `Source::open_simulating_removable`, so the file is really on the
//! internal SSD (in the page cache): it measures the code path, not a USB
//! drive. Both are budgeted at 150 ms in `src/budgets.rs`.
//! `open/first_paint` is the same open with no load, for comparison.

// Only `reference_file` is used here: one open takes milliseconds, and
// these set their own sampling rather than `whole_file`'s.
#[allow(dead_code)]
mod common;

use std::collections::VecDeque;
use std::hint::black_box;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use criterion::measurement::WallTime;
use criterion::{BenchmarkGroup, Criterion, SamplingMode, criterion_group, criterion_main};
use leal_core::document::{Document, FirstScreen, OpenOptions};
use leal_core::schedule::{Interval, JobHandle, Priority, Scheduler, SchedulerConfig};
use leal_core::source::{STREAM_CHUNK_BYTES, Source, TempFolders, VolumeInfo};

/// A screenful.
const OPTIONS: OpenOptions = OpenOptions {
    choices: leal_core::detect::Choices {
        delimiter: None,
        header: None,
        encoding: None,
    },
    first_screen_rows: 60,
    max_chars: 256,
};

/// The P1–P3 load described in the module docs.
struct Load {
    stop: Arc<AtomicBool>,
    opener: Option<thread::JoinHandle<()>>,
    busy: Vec<JobHandle<()>>,
    /// Chunks the P3 jobs have read, to check the load really ran.
    p3_chunks: Arc<AtomicU64>,
    /// Indexes the P1 loop has finished.
    p1_indexes: Arc<AtomicU64>,
}

impl Load {
    fn start(scheduler: &Scheduler, path: &Path, temp: &Path) -> Load {
        let bytes: Arc<[u8]> = Arc::from(std::fs::read(path).expect("reading the reference file"));
        let p3_chunks = Arc::new(AtomicU64::new(0));
        let busy = (0..scheduler.background_threads())
            .map(|_| {
                let (bytes, chunks) = (Arc::clone(&bytes), Arc::clone(&p3_chunks));
                scheduler.spawn(Priority::P3, Interval::Acceleration, move |job| {
                    let mut sum = 0u64;
                    loop {
                        for chunk in bytes.chunks(256 * 1024) {
                            job.checkpoint()?;
                            sum = chunk.iter().fold(sum, |s, &b| s.wrapping_add(u64::from(b)));
                            chunks.fetch_add(1, Ordering::Relaxed);
                        }
                        black_box(sum);
                    }
                })
            })
            .collect();
        let stop = Arc::new(AtomicBool::new(false));
        let p1_indexes = Arc::new(AtomicU64::new(0));
        let opener = thread::spawn({
            let (stop, indexes) = (Arc::clone(&stop), Arc::clone(&p1_indexes));
            let (scheduler, path) = (scheduler.clone(), path.to_owned());
            let temp = TempFolders::new(temp.join("load-scratch"), temp.join("load-records"));
            move || {
                while !stop.load(Ordering::Relaxed) {
                    let (document, _) = Document::open(
                        &path,
                        &temp,
                        VolumeInfo::default(),
                        &scheduler,
                        OPTIONS,
                        None,
                    )
                    .expect("opening the reference file");
                    if document.index_job().wait().is_ok() {
                        indexes.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        });
        Load {
            stop,
            opener: Some(opener),
            busy,
            p3_chunks,
            p1_indexes,
        }
    }

    /// How much P1 and P3 work the load has done so far.
    fn done(&self) -> (u64, u64) {
        (
            self.p1_indexes.load(Ordering::Relaxed),
            self.p3_chunks.load(Ordering::Relaxed),
        )
    }
}

/// The load, started the first time a loaded benchmark runs. A filter that
/// leaves out the loaded benchmarks (`just bench index/`) never starts it.
struct LazyLoad<'a> {
    scheduler: &'a Scheduler,
    path: &'a Path,
    dir: &'a Path,
    /// The load, and the work it had done when the measurements began.
    started: Option<(Load, (u64, u64))>,
}

impl LazyLoad<'_> {
    /// Starts the load if it isn't running yet.
    fn ensure(&mut self) {
        if self.started.is_none() {
            let load = Load::start(self.scheduler, self.path, self.dir);
            // Let the load get going.
            thread::sleep(Duration::from_millis(500));
            let done = load.done();
            self.started = Some((load, done));
        }
    }

    /// If the load ran, checks that every priority did some work while the
    /// loaded benchmarks were measured, then stops it.
    fn finish(self) {
        if let Some((load, (p1, p3))) = self.started {
            let (p1_now, p3_now) = load.done();
            assert!(p1_now > p1, "the P1 load didn't run");
            assert!(p3_now > p3, "the P3 load didn't run");
        }
    }
}

impl Drop for Load {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        for job in &self.busy {
            job.cancel();
        }
        if let Some(opener) = self.opener.take() {
            let _ = opener.join();
        }
    }
}

fn settings(group: &mut BenchmarkGroup<'_, WallTime>) {
    group
        .sampling_mode(SamplingMode::Flat)
        .sample_size(50)
        .warm_up_time(Duration::from_secs(2))
        .measurement_time(Duration::from_secs(5));
}

/// Times `open` to its first screen, `iters` times. The documents stay open
/// for two more opens, so their own index and review run alongside the
/// next measurements, and are dropped (cancelling their jobs) outside the
/// timed part.
fn time_opens(iters: u64, mut open: impl FnMut() -> (Document, FirstScreen)) -> Duration {
    let mut open_documents = VecDeque::new();
    let mut total = Duration::ZERO;
    for _ in 0..iters {
        let started = Instant::now();
        let (document, screen) = open();
        total += started.elapsed();
        assert_eq!(screen.rows.len(), OPTIONS.first_screen_rows);
        black_box(&screen);
        open_documents.push_back(document);
        if open_documents.len() > 2 {
            open_documents.pop_front();
        }
    }
    total
}

fn scratch() -> PathBuf {
    let dir = leal_bench::reference::data_dir().join("open-bench");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("making the benchmark's scratch folder");
    dir
}

fn open(c: &mut Criterion) {
    let path = common::reference_file();
    let dir = scratch();
    let temp = TempFolders::new(dir.join("scratch"), dir.join("records"));
    let scheduler = Scheduler::new(SchedulerConfig::default()).expect("starting the scheduler");

    let open_internal = || {
        Document::open(
            &path,
            &temp,
            VolumeInfo::default(),
            &scheduler,
            OPTIONS,
            None,
        )
        .expect("opening the reference file")
    };
    let open_removable = || {
        let source = Source::open_simulating_removable(&path, &temp, STREAM_CHUNK_BYTES)
            .expect("opening the reference file");
        Document::from_source(source, &scheduler, OPTIONS, None).expect("first paint")
    };

    let mut group = c.benchmark_group("open");
    settings(&mut group);

    // Criterion calls a benchmark's closure only if the filter selects it,
    // so these flags say what actually ran.
    let mut any_ran = false;
    group.bench_function("first_paint", |b| {
        any_ran = true;
        b.iter_custom(|iters| time_opens(iters, open_internal));
    });

    let mut load = LazyLoad {
        scheduler: &scheduler,
        path: &path,
        dir: &dir,
        started: None,
    };
    group.bench_function("first_paint_under_load", |b| {
        load.ensure();
        b.iter_custom(|iters| time_opens(iters, open_internal));
    });
    group.bench_function("first_paint_removable_under_load", |b| {
        load.ensure();
        b.iter_custom(|iters| time_opens(iters, open_removable));
    });
    any_ran |= load.started.is_some();
    load.finish();
    group.finish();
    if any_ran {
        report_chunks(&path, &temp);
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// Prints the longest stretch of work between two checkpoints in the index
/// and review jobs over the reference file, for DESIGN §3.10 rule 3 (jobs
/// work in chunks of at most ~5 ms, so they pause and cancel quickly). Not
/// a criterion result: the task notes record it.
///
/// The longest chunk is wall-clock time, so a busy machine (another
/// process taking the core) stretches it. Each run's longest is taken, and
/// the shortest of those five is printed: the quietest run, closest to the
/// work itself.
fn report_chunks(path: &Path, temp: &TempFolders) {
    let scheduler = Scheduler::new(SchedulerConfig::default()).expect("starting the scheduler");
    let mut index = Duration::MAX;
    let mut review = Duration::MAX;
    for _ in 0..5 {
        let (document, _) =
            Document::open(path, temp, VolumeInfo::default(), &scheduler, OPTIONS, None)
                .expect("opening the reference file");
        let (index_job, review_job) = (document.index_job(), document.review_job());
        let _ = index_job.wait();
        let _ = review_job.wait();
        index = index.min(index_job.control().longest_chunk());
        review = review.min(review_job.control().longest_chunk());
    }
    eprintln!(
        "open: longest chunk between checkpoints over the reference file (quietest of 5 runs): index {index:?}, review {review:?} (DESIGN §3.10 rule 3: ~5 ms)"
    );
}

criterion_group!(benches, open);
criterion_main!(benches);
