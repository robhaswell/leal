//! Documents on network shares (ADR-0009, PLAN 2.0), on a simulated share
//! (`Source::open_simulating_share`): first paint doesn't wait for a slow
//! share; rows come only from the copy, so the share is never read for a
//! row; a share that stops answering is disconnected, and one whose file
//! was deleted elsewhere says so; and the rows are the file's (F1, and the
//! corpus's own expectations).

use super::*;

use std::time::Instant;

use crate::source::{SimulatedShare, SimulatedShareFailure};
use leal_testkit::fidelity::assert_identical;

/// Retry waits short enough for tests.
const QUICK_RETRIES: &[Duration] = &[Duration::from_millis(1); 5];

fn open_share(
    dir: &Dir,
    bytes: &[u8],
    chunk: usize,
    share: SimulatedShare,
    scheduler: &Scheduler,
    progress: Option<ProgressCallback>,
) -> (Document, FirstScreen) {
    let path = dir.file("share.csv", bytes);
    let source = Source::open_simulating_share(&path, &dir.temp(), chunk, share).unwrap();
    assert!(source.is_on_network_share());
    Document::from_source(source, scheduler, options(30), progress).unwrap()
}

fn failing(at: usize, errno: i32, times: Option<u32>) -> SimulatedShare {
    SimulatedShare {
        failure: Some(SimulatedShareFailure {
            at,
            errno,
            times,
            on_stat: false,
            partial: false,
        }),
        retry_delays: QUICK_RETRIES,
        ..SimulatedShare::default()
    }
}

/// A slow share (every read takes 40 ms, and the copy is 40 reads, so at
/// least 1.6 s): first paint makes one read of it and returns, and doesn't
/// wait for the copy. Until the index pass brings them, rows past the
/// first 64 KB aren't served (the grid shows them as loading); then every
/// row is.
#[test]
fn first_paint_on_a_slow_share_does_not_wait_for_the_copy() {
    let dir = Dir::new("share-slow");
    let chunk = 64 * 1024;
    let bytes = sample(40 * chunk);
    let share = SimulatedShare {
        read_delay: Duration::from_millis(40),
        ..SimulatedShare::default()
    };
    let started = Instant::now();
    let (document, screen) = open_share(&dir, &bytes, chunk, share, &scheduler(), None);
    let opened = started.elapsed();
    let head_reads = document.source().simulated_head_reads();

    // One read of the share (40 ms), not forty (1.6 s). The bound is loose
    // for a loaded machine; the counts below are the exact check.
    assert!(
        opened < Duration::from_millis(800),
        "first paint took {opened:?}"
    );
    assert_eq!(head_reads, 1, "first paint read the share once");
    assert!(!document.index_job().control().is_finished());
    assert_eq!(document.storage(), Storage::Reading);

    let parser = document.current().parser;
    let all = expected_rows(&bytes, parser, 1000);
    assert_eq!(screen.rows, all[..30]);
    // Rows the index hasn't reached aren't served yet: loading.
    let head_rows = screen.row_count;
    assert!(head_rows < all.len() / 10);
    assert!(document.row_count() < all.len());
    assert!(
        document
            .rows(all.len() - 5..all.len(), 1000)
            .unwrap()
            .is_empty()
    );

    let summary = wait_for_index(&document);
    assert!(started.elapsed() >= Duration::from_millis(1600));
    assert_eq!(summary.rows, all.len());
    assert_eq!(document.rows(0..all.len(), 1000).unwrap(), all);
    assert_eq!(document.storage(), Storage::Copy);
    // First paint's read, and one per chunk of the copy.
    let chunks = bytes.len().div_ceil(chunk);
    assert_eq!(document.source().simulated_share_reads(), (1 + chunks, 0));
    assert_eq!(document.source().share_reads_on_main_thread(), 0);
}

/// What a progress report read: its row count, and the newest rows.
type RowsRead = (usize, Result<Vec<Vec<Cell>>, ReadErrorKind>);

/// Every row the index has is already in the internal copy, so rows are
/// never read from the share (where `read_range` would refuse with
/// `NotCopied`): from every progress report, the newest rows read, in
/// odd-sized chunks that cut rows, CRLFs and quotes.
#[test]
fn rows_of_a_share_are_read_from_the_copy_while_it_is_copied() {
    let dir = Dir::new("share-progress");
    let bytes = sample(300 * 1024);
    let gate = Gate::closed();
    let scheduler = scheduler_with(Arc::clone(&gate));
    let slot: Arc<Mutex<Option<Arc<Document>>>> = Arc::default();
    let read: Arc<Mutex<Vec<RowsRead>>> = Arc::default();
    let progress: ProgressCallback = Arc::new({
        let (slot, read) = (Arc::clone(&slot), Arc::clone(&read));
        move |p: IndexProgress| {
            if let Some(document) = slot.lock().unwrap().as_ref() {
                let start = p.rows.saturating_sub(20);
                let rows = document.rows(start..p.rows, 1000).map_err(|e| e.kind());
                read.lock().unwrap().push((p.rows, rows));
            }
        }
    });
    let share = SimulatedShare {
        retry_delays: QUICK_RETRIES,
        ..SimulatedShare::default()
    };
    let (document, _) = open_share(&dir, &bytes, 4093, share, &scheduler, Some(progress));
    let document = Arc::new(document);
    *slot.lock().unwrap() = Some(Arc::clone(&document));
    gate.open();
    wait_for_index(&document);
    *slot.lock().unwrap() = None;

    let all = expected_rows(&bytes, document.current().parser, 1000);
    let read = read.lock().unwrap();
    assert!(read.len() > 50, "only {} reports", read.len());
    for (rows, result) in read.iter() {
        let got = result
            .as_ref()
            .unwrap_or_else(|kind| panic!("at {rows} rows: {kind:?}"));
        assert_eq!(got, &all[rows.saturating_sub(20)..*rows]);
    }
}

/// A share that stops answering mid-copy (a network error that doesn't
/// pass): the index ends `Disconnected`, as for an unplugged drive, Save is
/// refused and the rows read stay. When the share answers again, the app's
/// check reconnects it and the copy completes.
#[test]
fn a_share_that_stops_answering_is_disconnected_until_it_is_back() {
    let dir = Dir::new("share-disconnect");
    let bytes = sample(300 * 1024);
    let (document, _) = open_share(
        &dir,
        &bytes,
        8192,
        failing(150 * 1024, libc::ETIMEDOUT, None),
        &scheduler(),
        None,
    );
    assert_eq!(
        document.index_job().control().wait_timeout(LONG),
        Some(Err(JobError::Read(ReadErrorKind::Disconnected)))
    );
    assert_eq!(document.storage(), Storage::Disconnected);
    assert!(!document.can_save());
    let rows = document.row_count();
    assert!(rows > 100);
    let all = expected_rows(&bytes, document.current().parser, 1000);
    assert_eq!(document.rows(0..rows, 1000).unwrap(), all[..rows]);
    assert_eq!(
        document.review_job().control().wait_timeout(LONG),
        Some(Err(JobError::Read(ReadErrorKind::Disconnected)))
    );

    // Still away: nothing changes.
    document.check_original();
    assert_eq!(document.storage(), Storage::Disconnected);
    assert_eq!(document.generation(), 0);
    // Back.
    document.source().simulate_drive_back();
    document.check_original();
    assert_eq!(document.generation(), 1);
    assert_eq!(wait_for_index(&document).rows, all.len());
    assert_eq!(document.storage(), Storage::Copy);
    assert!(document.can_save());
    assert_eq!(document.rows(0..all.len(), 1000).unwrap(), all);
}

/// Network errors that pass are invisible to the document: the index
/// completes and every row is right.
#[test]
fn a_share_that_recovers_from_network_errors_reads_every_row() {
    let dir = Dir::new("share-recover");
    let bytes = sample(300 * 1024);
    let (document, _) = open_share(
        &dir,
        &bytes,
        8192,
        failing(150 * 1024, libc::ECONNRESET, Some(4)),
        &scheduler(),
        None,
    );
    let all = expected_rows(&bytes, document.current().parser, 1000);
    assert_eq!(wait_for_index(&document).rows, all.len());
    assert_eq!(document.rows(0..all.len(), 1000).unwrap(), all);
    assert_eq!(document.storage(), Storage::Copy);
    assert!(document.can_save());
    assert_eq!(document.source().simulated_share_reads().1, 4);
}

/// A file deleted on its share by another computer (`ESTALE`): the index
/// ends `Deleted`, not `Disconnected`, the review too, Save is refused,
/// the rows read stay, and the app's check never reconnects it.
#[test]
fn a_file_deleted_on_its_share_is_reported_as_deleted() {
    let dir = Dir::new("share-deleted");
    let bytes = sample(300 * 1024);
    // The copy is held before the failing read, and the file deleted
    // meanwhile, as another computer would.
    let (document, _) = open_share(
        &dir,
        &bytes,
        8192,
        SimulatedShare {
            hold_at: Some(150 * 1024),
            ..failing(150 * 1024, libc::ESTALE, None)
        },
        &scheduler(),
        None,
    );
    std::fs::remove_file(dir.0.join("share.csv")).unwrap();
    document.source().simulated_share_release();
    assert_eq!(
        document.index_job().control().wait_timeout(LONG),
        Some(Err(JobError::Read(ReadErrorKind::Deleted)))
    );
    assert_eq!(document.storage(), Storage::Deleted);
    assert!(!document.can_save());
    assert_eq!(
        document.review_job().control().wait_timeout(LONG),
        Some(Err(JobError::Read(ReadErrorKind::Deleted)))
    );
    let rows = document.row_count();
    assert!(rows > 100);
    assert_eq!(document.rows(0..5, 100).unwrap().len(), 5);
    document.source().simulate_drive_back();
    document.check_original();
    assert_eq!(document.storage(), Storage::Deleted);
    assert_eq!(document.generation(), 0, "not read again");
}

/// A file on a share changed by another computer during the copy: the
/// share's watcher can't see it, but a look at the file can (the app looks
/// on activation, on mounts and while disconnected). Then the rest of the
/// copy would be the new version, so it is a change while reading: Save is
/// off, the copy stops, and the app offers Reload (task 2.0 review).
#[test]
fn a_share_file_changed_elsewhere_mid_copy_is_changed_while_reading() {
    let dir = Dir::new("share-changed");
    let bytes = sample(300 * 1024);
    let (document, _) = open_share(
        &dir,
        &bytes,
        8192,
        SimulatedShare {
            hold_at: Some(150 * 1024),
            retry_delays: QUICK_RETRIES,
            ..SimulatedShare::default()
        },
        &scheduler(),
        None,
    );
    let mut longer = bytes.clone();
    longer.extend_from_slice(b"9999999,more,rows\n");
    std::fs::write(dir.0.join("share.csv"), &longer).unwrap();
    let status = document.check_original();
    assert_eq!(status.state, OriginalState::Changed);
    assert!(document.changed_on_disk());
    assert!(!document.can_save());
    document.source().simulated_share_release();
    assert_eq!(
        document.index_job().control().wait_timeout(LONG),
        Some(Err(JobError::Read(ReadErrorKind::ChangedOnDisk)))
    );
}

/// The corpus through a share, in odd-sized chunks, read the way each
/// file's sidecar says: the row count and every row's field count are the
/// sidecar's (the testkit's oracle), the rows are those of the same file
/// opened on an internal volume, and the bytes are identical (F1).
#[test]
fn corpus_files_read_through_a_share_match_their_sidecars() {
    use crate::dialect::Delimiter as CoreDelimiter;
    use leal_testkit::dialect::Encoding as TkEncoding;
    let dir = Dir::new("share-corpus");
    let cases = leal_testkit::corpus::load().unwrap();
    assert!(!cases.is_empty());
    let scheduler = scheduler();
    for case in &cases {
        let expected = &case.sidecar;
        let choices = Choices {
            delimiter: CoreDelimiter::from_byte(expected.dialect.delimiter.byte()),
            header: Some(expected.dialect.header),
            encoding: Some(match expected.dialect.encoding {
                TkEncoding::Utf8 => Encoding::Utf8,
                TkEncoding::Utf16Le => Encoding::Utf16Le,
                TkEncoding::Utf16Be => Encoding::Utf16Be,
                TkEncoding::Windows1252 => Encoding::Windows1252,
            }),
        };
        let opening = OpenOptions {
            choices,
            ..options(30)
        };
        let file = case.name.replace('/', "-");
        let path = dir.file(&file, &case.bytes);
        let share = SimulatedShare {
            retry_delays: QUICK_RETRIES,
            ..SimulatedShare::default()
        };
        let source = Source::open_simulating_share(&path, &dir.temp(), 97, share).unwrap();
        let (document, _) = Document::from_source(source, &scheduler, opening, None)
            .unwrap_or_else(|error| panic!("{}: {error}", case.name));
        let (mapped, _) = Document::open(
            &path,
            &dir.temp(),
            VolumeInfo::default(),
            &scheduler,
            opening,
            None,
        )
        .unwrap();
        wait_for_index(&document);
        wait_for_index(&mapped);

        let rows = document.row_count();
        assert_eq!(rows, expected.rows.count, "{}: rows", case.name);
        let counts: Vec<usize> = document
            .cells(0..rows, 0..0, 1)
            .unwrap()
            .iter()
            .map(|row| row.field_count)
            .collect();
        assert_eq!(
            counts,
            expected.rows.field_counts(),
            "{}: fields",
            case.name
        );
        assert_eq!(
            document.rows(0..rows, 1000).unwrap(),
            mapped.rows(0..rows, 1000).unwrap(),
            "{}",
            case.name
        );
        assert_eq!(document.storage(), Storage::Copy, "{}", case.name);
        assert_identical(&case.bytes, document.source().as_slice().unwrap());
    }
}

/// The names of the threads that closed a simulated share's files.
static CLOSED_ON: Mutex<Vec<Option<String>>> = Mutex::new(Vec::new());

fn record_close() {
    let name = std::thread::current().name().map(str::to_owned);
    CLOSED_ON.lock().unwrap().push(name);
}

/// Dropping a document on a share closes both of its files on threads of
/// their own: the source's, and the watcher's (`O_EVTONLY`) descriptor,
/// which can be the last to close (task 2.0 re-review). Closing a file on a
/// share that has stopped answering can block.
#[test]
fn dropping_a_document_on_a_share_closes_its_files_on_their_own_threads() {
    let dir = Dir::new("share-close");
    let bytes = sample(100 * 1024);
    let (document, _) = open_share(
        &dir,
        &bytes,
        8192,
        SimulatedShare {
            on_close: Some(record_close),
            // Part-way through the copy, so the source still holds the
            // share's file (a complete copy lets go of it as it finishes).
            hold_at: Some(50 * 1024),
            retry_delays: QUICK_RETRIES,
            ..SimulatedShare::default()
        },
        &scheduler(),
        None,
    );
    drop(document);
    let deadline = Instant::now() + LONG;
    while CLOSED_ON.lock().unwrap().len() < 2 {
        assert!(
            Instant::now() < deadline,
            "closed: {:?}",
            CLOSED_ON.lock().unwrap()
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    let closed = CLOSED_ON.lock().unwrap().clone();
    assert_eq!(
        closed,
        [
            Some("leal-share-close".to_owned()),
            Some("leal-share-close".to_owned())
        ]
    );
}

/// `check_original` looks at the file: on a share, never on the main thread
/// (task 2.0 re-review). Tests never run on the main thread, so this
/// pretends: it is counted, and a debug build panics.
#[test]
fn checking_a_share_on_the_main_thread_is_caught() {
    use crate::source::PRETEND_MAIN_THREAD;
    use std::panic::{self, AssertUnwindSafe};
    let dir = Dir::new("share-check-main");
    let bytes = sample(50 * 1024);
    let share = SimulatedShare {
        retry_delays: QUICK_RETRIES,
        ..SimulatedShare::default()
    };
    let (document, _) = open_share(&dir, &bytes, 8192, share, &scheduler(), None);
    wait_for_index(&document);
    document.check_original();
    assert_eq!(document.source().share_reads_on_main_thread(), 0);
    PRETEND_MAIN_THREAD.with(|pretend| pretend.set(true));
    let result = panic::catch_unwind(AssertUnwindSafe(|| {
        document.check_original();
    }));
    PRETEND_MAIN_THREAD.with(|pretend| pretend.set(false));
    assert_eq!(
        result.is_err(),
        cfg!(debug_assertions),
        "debug builds panic"
    );
    assert_eq!(document.source().share_reads_on_main_thread(), 1);
}
