//! Tests for the document FFI: what Swift sees of opening a document,
//! reading rows, progress, cancelling and waiting for jobs.

use super::*;

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::Wake;
use std::thread;
use std::time::{Duration, Instant};

/// A temporary directory that is deleted when the test ends.
struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("leal-ffi-document-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn file(&self, name: &str, bytes: &[u8]) -> String {
        let path = self.0.join(name);
        std::fs::write(&path, bytes).unwrap();
        path.to_string_lossy().into_owned()
    }

    fn locations(&self) -> TempLocations {
        TempLocations {
            scratch_dir: self.0.join("scratch").to_string_lossy().into_owned(),
            records_dir: self.0.join("records").to_string_lossy().into_owned(),
        }
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Wakes the thread that is blocked in [`block_on`].
struct Unpark(thread::Thread);

impl Wake for Unpark {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }
}

/// Runs a future to completion on this thread, the way an executor would:
/// poll, and park until woken. A future that is never woken would hang,
/// so this gives up after a while.
fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = std::pin::pin!(future);
    let waker = Waker::from(Arc::new(Unpark(thread::current())));
    let mut context = Context::from_waker(&waker);
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Poll::Ready(output) = future.as_mut().poll(&mut context) {
            return output;
        }
        assert!(Instant::now() < deadline, "the future was never woken");
        thread::park_timeout(Duration::from_secs(1));
    }
}

fn options() -> OpenOptions {
    OpenOptions {
        delimiter: None,
        header: None,
        encoding: None,
        first_screen_rows: 10,
        max_chars: 100,
    }
}

/// Counts progress reports and keeps the last.
#[derive(Default)]
struct Observer {
    reports: AtomicU64,
    last: Mutex<Option<IndexProgress>>,
}

impl ProgressObserver for Observer {
    fn index_progressed(&self, progress: IndexProgress) {
        self.reports.fetch_add(1, Ordering::SeqCst);
        *self.last.lock().unwrap() = Some(progress);
    }
}

fn text(rows: &[Vec<Cell>]) -> Vec<Vec<&str>> {
    rows.iter()
        .map(|row| row.iter().map(|c| c.text.as_str()).collect())
        .collect()
}

#[test]
fn open_document_gives_the_first_screen_then_rows() {
    let dir = TempDir::new("open");
    let path = dir.file(
        "a.csv",
        "name;city\nZoë;\"Zürich\nCH\"\nAda;London\n".as_bytes(),
    );
    let scheduler = Scheduler::new().unwrap();
    assert!(scheduler.background_threads() >= 1);
    let observer = Arc::new(Observer::default());
    let document = open_document(
        &path,
        VolumeInfo::default(),
        dir.locations(),
        &scheduler,
        options(),
        Some(Arc::clone(&observer) as Arc<dyn ProgressObserver>),
    )
    .unwrap();

    let screen = document.first_screen().unwrap();
    assert_eq!(screen.generation, 0);
    assert_eq!(
        text(&screen.rows),
        [["name", "city"], ["Zoë", "Zürich\nCH"], ["Ada", "London"]]
    );
    assert_eq!((screen.row_count, screen.estimated_row_count), (3, 3));
    assert_eq!(
        screen.interpretation,
        Interpretation {
            encoding: TextEncoding::Utf8,
            encoding_source: EncodingSource::Guess,
            delimiter: Delimiter::Semicolon,
            delimiter_source: DialectSource::Guess,
            header: true,
            header_source: DialectSource::Guess,
            line_ending: Some(LineEnding::Lf),
        }
    );
    assert_eq!(screen.column_count, 2);
    assert_eq!(document.column_count().unwrap(), 2);
    assert_eq!(document.storage().unwrap(), SourceStorage::Clone);

    // The jobs finish, and Swift's await is woken.
    assert_eq!(block_on(document.index_job().unwrap().wait()), Ok(()));
    assert_eq!(block_on(document.review_job().unwrap().wait()), Ok(()));
    assert!(document.index_job().unwrap().is_finished());
    assert_eq!(
        document.review().unwrap(),
        Some(ReviewResult {
            encoding_suggestion: None,
            delimiter_suggestion: None,
            line_ending: Some(LineEnding::Lf),
        })
    );
    assert_eq!(document.row_count().unwrap(), 3);
    assert_eq!(document.estimated_row_count().unwrap(), 3);
    assert!(document.progress().unwrap().complete);
    assert!(observer.reports.load(Ordering::SeqCst) >= 1);
    assert_eq!(
        *observer.last.lock().unwrap(),
        Some(IndexProgress {
            generation: 0,
            rows: 3,
            estimated_rows: 3,
            bytes_scanned: 39,
            bytes_total: 39,
            complete: true,
        })
    );

    let rows = document.rows(1, 5, 3).unwrap();
    assert_eq!(text(&rows), [["Zoë", "Zür"], ["Ada", "Lon"]]);
    assert!(!rows[0][0].truncated && rows[0][1].truncated);
    assert_eq!(document.rows(99, 5, 3).unwrap(), Vec::<Vec<Cell>>::new());

    // The grid reads a window of columns, with each row's field count.
    let window = document.cells(0, 5, 1, 4, 3).unwrap();
    assert_eq!(
        window
            .iter()
            .map(|row| (
                row.field_count,
                row.cells.iter().map(|c| c.text.as_str()).collect()
            ))
            .collect::<Vec<(u32, Vec<&str>)>>(),
        [(2, vec!["cit"]), (2, vec!["Zür"]), (2, vec!["Lon"])]
    );
    assert_eq!(document.numeric_columns(10).unwrap(), [false, false]);

    // Treat as comma-separated: read again, without reopening.
    let comma = OpenOptions {
        delimiter: Some(Delimiter::Comma),
        ..options()
    };
    let screen = document.reinterpret(comma).unwrap();
    assert_eq!(screen.generation, 1);
    assert_eq!(screen.interpretation.delimiter, Delimiter::Comma);
    assert_eq!(screen.interpretation.delimiter_source, DialectSource::User);
    assert_eq!(document.first_screen().unwrap(), screen);
    assert_eq!(text(&screen.rows)[0], ["name;city"]);
    assert_eq!(block_on(document.index_job().unwrap().wait()), Ok(()));
    assert_eq!(
        document.interpretation().unwrap().delimiter,
        Delimiter::Comma
    );
}

#[test]
fn a_cancelled_job_says_so() {
    let dir = TempDir::new("cancel");
    // Large enough that the review is still running when it is cancelled.
    let path = dir.file("big.csv", &b"a,b\n1,2\n".repeat(1 << 20));
    let scheduler = Scheduler::new().unwrap();
    // Hold background work, so the review can't finish first.
    scheduler.set_interacting(true);
    let document = open_document(
        &path,
        VolumeInfo::default(),
        dir.locations(),
        &scheduler,
        options(),
        None,
    )
    .unwrap();
    let review = document.review_job().unwrap();
    review.cancel();
    assert_eq!(block_on(review.wait()), Err(JobFailure::Cancelled));
    assert_eq!(document.review().unwrap(), None);
    scheduler.set_interacting(false);
    scheduler.note_user_input();
    assert_eq!(block_on(document.index_job().unwrap().wait()), Ok(()));
}

#[test]
fn waiting_on_a_finished_job_is_ready_at_once() {
    let dir = TempDir::new("finished");
    let path = dir.file("a.csv", b"a\n");
    let scheduler = Scheduler::new().unwrap();
    let document = open_document(
        &path,
        VolumeInfo::default(),
        dir.locations(),
        &scheduler,
        options(),
        None,
    )
    .unwrap();
    let job = document.index_job().unwrap();
    assert_eq!(block_on(job.wait()), Ok(()));
    assert_eq!(block_on(job.wait()), Ok(()));
    assert!(job.id() > 0);
}

#[test]
fn errors_reach_swift_as_lealerror() {
    let dir = TempDir::new("errors");
    let scheduler = Scheduler::new().unwrap();
    let missing = dir.0.join("missing.csv").to_string_lossy().into_owned();
    let open = |path: &str, options: OpenOptions| {
        open_document(
            path,
            VolumeInfo::default(),
            dir.locations(),
            &scheduler,
            options,
            None,
        )
        .map(|_| ())
    };
    assert_eq!(
        open(&missing, options()),
        Err(LealError::NotFound {
            path: missing.clone(),
            code: Some(2)
        })
    );
    let bom = dir.file("bom.csv", b"\xEF\xBB\xBFa,b\n");
    let utf16 = OpenOptions {
        encoding: Some(TextEncoding::Utf16Le),
        ..options()
    };
    assert_eq!(
        open(&bom, utf16),
        Err(LealError::EncodingDoesNotFit { path: bom.clone() })
    );
    assert_eq!(
        document_error("x.csv", DocumentError::TooLarge { len: 1 << 33 }),
        LealError::TooLarge {
            path: "x.csv".to_owned(),
            byte_count: 1 << 33
        }
    );
}

#[test]
fn job_failures_convert() {
    assert_eq!(JobFailure::from(JobError::Cancelled), JobFailure::Cancelled);
    assert_eq!(
        JobFailure::from(JobError::Read(ReadErrorKind::Disconnected)),
        JobFailure::DriveDisconnected
    );
    assert_eq!(
        JobFailure::from(JobError::Read(ReadErrorKind::ChangedOnDisk)),
        JobFailure::ChangedOnDisk
    );
    assert_eq!(
        JobFailure::from(JobError::Panicked("boom".to_owned())),
        JobFailure::Panicked {
            message: "boom".to_owned()
        }
    );
    assert!(matches!(
        JobFailure::from(JobError::Failed("no thread".to_owned())),
        JobFailure::Failed { message } if message == "no thread"
    ));
}

#[test]
fn every_encoding_and_delimiter_converts_both_ways() {
    for encoding in dialect::Encoding::ALL {
        let ffi = TextEncoding::from(encoding);
        assert_eq!(dialect::Encoding::from(ffi), encoding);
        assert_eq!(format!("{ffi:?}"), format!("{encoding:?}"));
    }
    for delimiter in dialect::Delimiter::ALL {
        assert_eq!(
            dialect::Delimiter::from(Delimiter::from(delimiter)),
            delimiter
        );
    }
}

/// Diagnostics (task 1.5) reach Swift: the report with kinds, severities,
/// counts and locations, and the row marks.
#[test]
fn diagnostics_reach_swift() {
    let dir = TempDir::new("diagnostics");
    // Row 2 is ragged, row 3 has text after a closing quote and a NUL,
    // row 4 is blank.
    let path = dir.file("messy.csv", b"a,b\n1,2\n3\n\"x\"y,\0\n\n5,6\n");
    let scheduler = Scheduler::new().unwrap();
    let document = open_document(
        &path,
        VolumeInfo::default(),
        dir.locations(),
        &scheduler,
        options(),
        None,
    )
    .unwrap();
    assert_eq!(block_on(document.index_job().unwrap().wait()), Ok(()));
    let report = document.diagnostics().unwrap();
    let at = |row, offset| DiagnosticLocation { row, offset };
    assert_eq!(
        report,
        DiagnosticsReport {
            generation: 0,
            rows: 6,
            complete: true,
            diagnostics: vec![
                Diagnostic {
                    kind: DiagnosticKind::RaggedRows,
                    severity: Severity::Warning,
                    count: 1,
                    first: vec![at(2, 8)],
                },
                Diagnostic {
                    kind: DiagnosticKind::TextAfterClosingQuote,
                    severity: Severity::Warning,
                    count: 1,
                    first: vec![at(3, 13)],
                },
                Diagnostic {
                    kind: DiagnosticKind::NulBytes,
                    severity: Severity::Warning,
                    count: 1,
                    first: vec![at(3, 15)],
                },
                Diagnostic {
                    kind: DiagnosticKind::BlankLines,
                    severity: Severity::Info,
                    count: 1,
                    first: vec![at(4, 17)],
                },
            ],
            shows_banner: true,
            banner_kinds: 3,
        }
    );
    let marks: Vec<bool> = (0..7)
        .map(|r| document.row_has_diagnostic(r).unwrap())
        .collect();
    assert_eq!(marks, [false, false, true, true, false, false, false]);
    assert_eq!(document.next_row_with_diagnostic(0).unwrap(), Some(2));
    assert_eq!(document.next_row_with_diagnostic(4).unwrap(), None);
    assert_eq!(document.previous_row_with_diagnostic(6).unwrap(), Some(3));
    assert_eq!(document.previous_row_with_diagnostic(2).unwrap(), None);
    assert!(!document.row_has_diagnostic(u64::MAX).unwrap());
}

#[test]
fn every_diagnostic_kind_and_severity_converts() {
    use leal_core::diagnostics::DiagnosticKind as Core;
    for kind in Core::ALL {
        let ffi = DiagnosticKind::from(kind);
        assert_eq!(format!("{ffi:?}"), format!("{kind:?}"));
        assert_eq!(
            format!("{:?}", Severity::from(kind.severity())),
            format!("{:?}", kind.severity())
        );
    }
}

fn small_document(dir: &TempDir, scheduler: &Scheduler) -> Arc<Document> {
    let path = dir.file("small.csv", b"a,b\n1,2\n");
    open_document(
        &path,
        VolumeInfo::default(),
        dir.locations(),
        scheduler,
        options(),
        None,
    )
    .unwrap()
}

/// DESIGN §3.9: after a panic in one of its calls, the document has failed,
/// and every later call says so instead of touching it.
#[test]
fn a_panic_in_a_call_fails_the_document() {
    let dir = TempDir::new("panic-call");
    let scheduler = Scheduler::new().unwrap();
    let document = small_document(&dir, &scheduler);
    assert!(!document.is_failed());
    let result = document.call(|| -> Result<(), LealError> { panic!("deliberate") });
    let failed = LealError::DocumentFailed {
        path: document.path.clone(),
        message: "deliberate".to_owned(),
    };
    assert_eq!(result, Err(failed.clone()));
    assert!(document.is_failed());
    assert_eq!(document.rows(0, 5, 10), Err(failed.clone()));
    assert_eq!(document.row_count(), Err(failed.clone()));
    assert_eq!(document.first_screen().map(|_| ()), Err(failed.clone()));
    assert_eq!(document.index_job().map(|_| ()), Err(failed.clone()));
    assert_eq!(document.reinterpret(options()).map(|_| ()), Err(failed));
}

/// A panic in one of the document's jobs fails it too, and the job's wait
/// says it panicked.
#[test]
fn a_panic_in_a_job_fails_the_document() {
    let dir = TempDir::new("panic-job");
    let scheduler = Scheduler::new().unwrap();
    let document = small_document(&dir, &scheduler);
    let handle: schedule::JobHandle<()> =
        scheduler
            .scheduler
            .spawn(schedule::Priority::P2, schedule::Interval::Review, |_| {
                panic!("deliberate job panic")
            });
    document.failure.watch(handle.control());
    let job = Job {
        control: handle.control().clone(),
    };
    assert_eq!(
        block_on(job.wait()),
        Err(JobFailure::Panicked {
            message: "deliberate job panic".to_owned()
        })
    );
    // `on_finish` runs on the job's thread as it finishes; give it a moment.
    let deadline = Instant::now() + Duration::from_secs(10);
    while !document.is_failed() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(1));
    }
    assert!(document.is_failed());
    assert!(matches!(
        document.rows(0, 1, 1),
        Err(LealError::DocumentFailed { message, .. }) if message.contains("deliberate job panic")
    ));
}

/// A job that ends normally, or is cancelled, doesn't fail the document.
#[test]
fn jobs_that_end_normally_leave_the_document_working() {
    let dir = TempDir::new("no-panic");
    let scheduler = Scheduler::new().unwrap();
    let document = small_document(&dir, &scheduler);
    assert_eq!(block_on(document.index_job().unwrap().wait()), Ok(()));
    let review = document.review_job().unwrap();
    review.cancel();
    let _ = block_on(review.wait());
    assert!(!document.is_failed());
    assert_eq!(document.row_count(), Ok(2));
}
