//! Tests for opening documents: first paint before the index, the
//! background jobs, rows from every source, re-reading with other choices,
//! cancellation and instrumentation.
//!
//! Tests that need the index not to have started yet hold its thread at
//! the start ([`Gate`]), so they don't depend on timing: if first paint
//! waited for the index, they would time out instead.

use super::*;

use std::path::PathBuf;
use std::sync::Condvar;
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::mpsc;
use std::time::Duration;

use crate::detect::{EncodingSource, REVIEW_CHUNK_BYTES};
use crate::diagnostics::{DiagnosticKind, MAX_LOCATIONS};
use crate::dialect::{Delimiter, Encoding};
use crate::schedule::{Platform, SchedulerConfig, ThreadClass};

const LONG: Duration = Duration::from_secs(30);

/// A temporary folder for one test, deleted at the end.
struct Dir(PathBuf);

impl Dir {
    fn new(name: &str) -> Dir {
        let dir = std::env::temp_dir().join(format!(
            "leal-document-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Dir(dir)
    }

    fn file(&self, name: &str, bytes: &[u8]) -> PathBuf {
        let path = self.0.join(name);
        std::fs::write(&path, bytes).unwrap();
        path
    }

    fn temp(&self) -> TempFolders {
        TempFolders::new(self.0.join("scratch"), self.0.join("records"))
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A platform that can hold index threads at their start, and records the
/// intervals it is told about.
#[derive(Default)]
struct Gate {
    closed: Mutex<bool>,
    opened: Condvar,
    intervals: Mutex<Vec<(Interval, u64, bool)>>,
}

impl Gate {
    fn closed() -> Arc<Gate> {
        let gate = Arc::new(Gate::default());
        *gate.closed.lock().unwrap() = true;
        gate
    }

    fn open(&self) {
        *self.closed.lock().unwrap() = false;
        self.opened.notify_all();
    }

    fn intervals(&self) -> Vec<(Interval, u64, bool)> {
        self.intervals.lock().unwrap().clone()
    }
}

impl Platform for Gate {
    fn thread_started(&self, class: ThreadClass) {
        if class == ThreadClass::Index {
            let mut closed = self.closed.lock().unwrap();
            while *closed {
                closed = self.opened.wait(closed).unwrap();
            }
        }
    }

    fn begin(&self, interval: Interval, id: u64) {
        self.intervals.lock().unwrap().push((interval, id, true));
    }

    fn end(&self, interval: Interval, id: u64) {
        self.intervals.lock().unwrap().push((interval, id, false));
    }
}

fn scheduler_with(platform: Arc<Gate>) -> Scheduler {
    Scheduler::new(SchedulerConfig {
        platform,
        background_threads: Some(2),
        ..SchedulerConfig::default()
    })
    .unwrap()
}

fn scheduler() -> Scheduler {
    scheduler_with(Arc::new(Gate::default()))
}

fn options(rows: usize) -> OpenOptions {
    OpenOptions {
        first_screen_rows: rows,
        max_chars: 1000,
        ..OpenOptions::default()
    }
}

/// Every row of `bytes` as the grid would show it, from the whole file.
fn expected_rows(bytes: &[u8], parser: RowParser, max_chars: usize) -> Vec<Vec<Cell>> {
    let index = RowIndex::build(bytes, parser.dialect()).unwrap();
    (0..index.row_count())
        .map(|r| {
            let row = parser.parse_row(&index, r, bytes).unwrap();
            cells(&parser, bytes, 0, row.fields(), max_chars)
        })
        .collect()
}

fn text(rows: &[Vec<Cell>]) -> Vec<Vec<&str>> {
    rows.iter()
        .map(|row| row.iter().map(|c| c.text.as_str()).collect())
        .collect()
}

/// A CSV file of about `size` bytes: a header, then numbered rows with a
/// quoted field holding a comma, a quote and a newline now and then.
fn sample(size: usize) -> Vec<u8> {
    let mut bytes = b"id,name,notes\n".to_vec();
    let mut i = 0;
    while bytes.len() < size {
        let notes = match i % 7 {
            0 => "\"two\nlines\"".to_owned(),
            3 => "\"a, \"\"b\"\"\"".to_owned(),
            _ => format!("n{i}"),
        };
        bytes.extend_from_slice(format!("{i},caf\u{e9} {i},{notes}\n").as_bytes());
        i += 1;
    }
    bytes
}

fn wait_for_index(document: &Document) -> IndexSummary {
    let job = document.index_job();
    assert_eq!(job.control().wait_timeout(LONG), Some(Ok(())));
    *job.wait().unwrap()
}

// ---------------------------------------------------------------------------
// First paint (DESIGN §3.10 rule 1)

#[test]
fn first_paint_does_not_wait_for_the_index() {
    let dir = Dir::new("first-paint");
    let bytes = sample(400 * 1024);
    let path = dir.file("a.csv", &bytes);
    let gate = Gate::closed();
    let scheduler = scheduler_with(Arc::clone(&gate));

    // Open on another thread, so a first paint that waited for the
    // (held) index would show up as a timeout, not a hang.
    let (sent, opened) = mpsc::channel();
    let temp = dir.temp();
    let opener = std::thread::spawn({
        let scheduler = scheduler.clone();
        move || {
            let result = Document::open(
                &path,
                &temp,
                VolumeInfo::default(),
                &scheduler,
                options(20),
                None,
            );
            sent.send(result.map_err(|e| e.to_string())).unwrap();
        }
    });
    let (document, screen) = opened.recv_timeout(LONG).expect("first paint").unwrap();
    opener.join().unwrap();

    // The first screen came from the first 64 KB, with the index held.
    assert!(!document.index_job().control().is_finished());
    assert_eq!(document.progress().bytes_scanned, 0);
    let parser = document.current().parser;
    let all = expected_rows(&bytes, parser, 1000);
    assert_eq!(screen.rows, all[..20]);
    assert_eq!(screen.detection.delimiter, Delimiter::Comma);
    assert_eq!(screen.detection.encoding, Encoding::Utf8);
    assert!(screen.detection.header);
    assert_eq!(screen.generation, 0);
    // Every whole row in the first 64 KB can be read, and no more yet.
    let head_rows = screen.row_count;
    assert!(head_rows > 20 && head_rows < all.len());
    assert_eq!(document.row_count(), head_rows);
    assert_eq!(
        document.rows(0..head_rows + 50, 1000).unwrap(),
        all[..head_rows]
    );
    assert_eq!(
        document.rows(head_rows..head_rows + 5, 1000).unwrap(),
        Vec::<Vec<Cell>>::new()
    );
    // The scrollbar's estimate is close. (The sample's rows get longer as
    // their numbers do, so the first 64 KB overestimate a little.)
    let estimate = screen.estimated_row_count as f64;
    assert!(
        (estimate / all.len() as f64 - 1.0).abs() < 0.15,
        "{estimate} vs {}",
        all.len()
    );
    assert_eq!(document.estimated_row_count(), screen.estimated_row_count);

    // Let the index run: every row, an exact count.
    gate.open();
    let summary = wait_for_index(&document);
    assert_eq!(summary.rows, all.len());
    assert_eq!(summary.field_count_mode, Some(3));
    assert_eq!(document.row_count(), all.len());
    assert_eq!(document.estimated_row_count(), all.len());
    assert_eq!(document.rows(0..all.len(), 1000).unwrap(), all);
    assert_eq!(
        document.rows(all.len() - 3..all.len() + 3, 1000).unwrap(),
        all[all.len() - 3..]
    );
    assert!(document.progress().complete);

    // First paint's interval ended before the index's began.
    let intervals = gate.intervals();
    let first_paint_end = intervals
        .iter()
        .position(|&(i, _, begin)| i == Interval::FirstPaint && !begin)
        .unwrap();
    let index_begin = intervals
        .iter()
        .position(|&(i, _, begin)| i == Interval::Index && begin)
        .unwrap();
    assert!(first_paint_end < index_begin, "{intervals:?}");
}

#[test]
fn a_file_that_fits_in_64_kb_is_known_at_first_paint() {
    let dir = Dir::new("small");
    let bytes = b"\xEF\xBB\xBFa;b\r\n1;\"x\r\ny\"\r\n2;z";
    let path = dir.file("small.csv", bytes);
    let gate = Gate::closed();
    let scheduler = scheduler_with(Arc::clone(&gate));
    let (document, screen) = Document::open(
        &path,
        &dir.temp(),
        VolumeInfo::default(),
        &scheduler,
        options(10),
        None,
    )
    .unwrap();
    assert_eq!(
        text(&screen.rows),
        [["a", "b"], ["1", "x\r\ny"], ["2", "z"]]
    );
    assert_eq!(screen.detection.delimiter, Delimiter::Semicolon);
    assert_eq!(screen.detection.encoding_source, EncodingSource::Bom);
    assert_eq!((screen.row_count, screen.estimated_row_count), (3, 3));
    assert_eq!(
        document.rows(1..3, 1).unwrap()[0][1],
        Cell {
            text: "x".to_owned(),
            truncated: true
        }
    );
    gate.open();
    assert_eq!(wait_for_index(&document).rows, 3);
}

#[test]
fn an_empty_file_opens_with_no_rows() {
    let dir = Dir::new("empty");
    let path = dir.file("empty.csv", b"");
    let (document, screen) = Document::open(
        &path,
        &dir.temp(),
        VolumeInfo::default(),
        &scheduler(),
        options(10),
        None,
    )
    .unwrap();
    assert!(screen.rows.is_empty());
    assert_eq!(wait_for_index(&document).rows, 0);
    assert_eq!(document.rows(0..10, 10).unwrap(), Vec::<Vec<Cell>>::new());
    assert_eq!(
        document.review_job().control().wait_timeout(LONG),
        Some(Ok(()))
    );
}

#[test]
fn utf16_files_open_with_their_bom() {
    let dir = Dir::new("utf16");
    let mut bytes = vec![0xFF, 0xFE];
    for unit in "név,város\n\u{0A22}\u{220A},\"a\nb\"\n".encode_utf16() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    let path = dir.file("u.csv", &bytes);
    let (document, screen) = Document::open(
        &path,
        &dir.temp(),
        VolumeInfo::default(),
        &scheduler(),
        options(10),
        None,
    )
    .unwrap();
    assert_eq!(screen.detection.encoding, Encoding::Utf16Le);
    assert_eq!(
        text(&screen.rows),
        [["név", "város"], ["\u{0A22}\u{220A}", "a\nb"]]
    );
    wait_for_index(&document);
    assert_eq!(document.rows(0..2, 100).unwrap(), screen.rows);
}

#[test]
fn a_chosen_encoding_must_fit_the_bom() {
    let dir = Dir::new("choice");
    let path = dir.file("bom.csv", b"\xEF\xBB\xBFa,b\n");
    let result = Document::open(
        &path,
        &dir.temp(),
        VolumeInfo::default(),
        &scheduler(),
        OpenOptions {
            choices: Choices {
                encoding: Some(Encoding::Utf16Be),
                ..Choices::default()
            },
            ..OpenOptions::default()
        },
        None,
    );
    assert!(matches!(result, Err(DocumentError::Choice(_))));
    assert!(matches!(
        Document::open(
            &dir.0.join("missing.csv"),
            &dir.temp(),
            VolumeInfo::default(),
            &scheduler(),
            OpenOptions::default(),
            None
        ),
        Err(DocumentError::Open(_))
    ));
}

// ---------------------------------------------------------------------------
// Background jobs

#[test]
fn progress_is_reported_as_the_index_grows() {
    let dir = Dir::new("progress");
    let bytes = sample(3 * crate::index::DIAGNOSTICS_CHUNK_BYTES + 1000);
    let path = dir.file("p.csv", &bytes);
    let (sent, reports) = mpsc::channel();
    let sent = Mutex::new(sent);
    let progress: ProgressCallback = Arc::new(move |p| sent.lock().unwrap().send(p).unwrap());
    let (document, _) = Document::open(
        &path,
        &dir.temp(),
        VolumeInfo::default(),
        &scheduler(),
        options(5),
        Some(progress),
    )
    .unwrap();
    let rows = wait_for_index(&document).rows;
    let reports: Vec<IndexProgress> = reports.try_iter().collect();
    assert_eq!(reports.len(), 4, "one per 256 KiB chunk");
    assert!(reports.windows(2).all(|w| w[0].rows <= w[1].rows));
    assert!(reports.iter().all(|p| p.generation == 0));
    assert!(
        reports[..3]
            .iter()
            .all(|p| !p.complete && p.estimated_rows > p.rows)
    );
    let last = reports.last().unwrap();
    assert!(last.complete);
    assert_eq!((last.rows, last.estimated_rows), (rows, rows));
    assert_eq!(last.bytes_scanned, last.bytes_total);
    // The index thread's chunks were well under DESIGN §3.10's 5 ms, even
    // in a debug build.
    assert!(document.index_job().control().longest_chunk() < Duration::from_millis(500));
}

#[test]
fn the_review_runs_alongside_the_index_and_suggests() {
    let dir = Dir::new("review");
    // ASCII for the first 64 KB, then Windows-1252: first paint guesses
    // UTF-8, and the review suggests Windows-1252 (ADR-0005 decision 4).
    let mut bytes = b"id,name\n".repeat(FIRST_PAINT_BYTES / 8 + 100);
    bytes.extend_from_slice(b"99,caf\xE9\n");
    let path = dir.file("r.csv", &bytes);
    let (document, screen) = Document::open(
        &path,
        &dir.temp(),
        VolumeInfo::default(),
        &scheduler(),
        options(5),
        None,
    )
    .unwrap();
    assert_eq!(screen.detection.encoding, Encoding::Utf8);
    let review = document.review_job();
    let review = review.wait().unwrap();
    assert_eq!(review.encoding_suggestion, Some(Encoding::Windows1252));
    // Nothing was re-decided.
    assert_eq!(document.detection().encoding, Encoding::Utf8);
}

#[test]
fn the_review_pauses_for_the_user_but_the_index_does_not() {
    let dir = Dir::new("pause");
    let bytes = sample(64 * REVIEW_CHUNK_BYTES);
    let path = dir.file("big.csv", &bytes);
    let scheduler = scheduler();
    scheduler.set_interacting(true);
    let (document, _) = Document::open(
        &path,
        &dir.temp(),
        VolumeInfo::default(),
        &scheduler,
        options(5),
        None,
    )
    .unwrap();
    wait_for_index(&document);
    let review = document.review_job();
    assert_eq!(
        review.control().wait_timeout(Duration::from_millis(300)),
        None
    );
    scheduler.set_interacting(false);
    assert_eq!(review.control().wait_timeout(LONG), Some(Ok(())));
}

#[test]
fn cancelling_the_index_keeps_the_rows_found_so_far() {
    let dir = Dir::new("cancel");
    let bytes = sample(200 * 1024);
    let path = dir.file("c.csv", &bytes);
    let gate = Gate::closed();
    let scheduler = scheduler_with(Arc::clone(&gate));
    let (document, screen) = Document::open(
        &path,
        &dir.temp(),
        VolumeInfo::default(),
        &scheduler,
        options(5),
        None,
    )
    .unwrap();
    document.index_job().cancel();
    gate.open();
    assert_eq!(
        document.index_job().control().wait_timeout(LONG),
        Some(Err(JobError::Cancelled))
    );
    // The first 64 KB's rows still read.
    assert_eq!(document.row_count(), screen.row_count);
    assert_eq!(document.rows(0..5, 1000).unwrap(), screen.rows);
    assert!(!document.progress().complete);
}

#[test]
fn dropping_a_document_cancels_its_jobs() {
    let dir = Dir::new("drop");
    let bytes = sample(64 * REVIEW_CHUNK_BYTES);
    let path = dir.file("d.csv", &bytes);
    let gate = Gate::closed();
    let scheduler = scheduler_with(Arc::clone(&gate));
    scheduler.set_interacting(true);
    let (document, _) = Document::open(
        &path,
        &dir.temp(),
        VolumeInfo::default(),
        &scheduler,
        options(5),
        None,
    )
    .unwrap();
    let (index, review) = (document.index_job(), document.review_job());
    drop(document);
    gate.open();
    assert_eq!(
        index.control().wait_timeout(LONG),
        Some(Err(JobError::Cancelled))
    );
    assert_eq!(
        review.control().wait_timeout(LONG),
        Some(Err(JobError::Cancelled))
    );
    // Once the jobs have let go of the file, its clone is gone.
    let scratch = dir.0.join("scratch");
    let deadline = std::time::Instant::now() + LONG;
    while std::fs::read_dir(&scratch).is_ok_and(|mut d| d.next().is_some())
        && std::time::Instant::now() < deadline
    {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(std::fs::read_dir(&scratch).is_ok_and(|mut d| d.next().is_none()));
}

// ---------------------------------------------------------------------------
// Re-reading with other choices (PLAN 1.3, ADR-0005 decision 8)

#[test]
fn reinterpreting_reads_the_file_again_without_reopening_it() {
    let dir = Dir::new("reinterpret");
    let bytes = b"a;b,c\n1;\"2,3\"\n";
    let path = dir.file("r.csv", bytes);
    let (sent, reports) = mpsc::channel();
    let sent = Mutex::new(sent);
    let progress: ProgressCallback = Arc::new(move |p| sent.lock().unwrap().send(p).unwrap());
    let (document, screen) = Document::open(
        &path,
        &dir.temp(),
        VolumeInfo::default(),
        &scheduler(),
        options(10),
        Some(progress),
    )
    .unwrap();
    assert_eq!(screen.detection.delimiter, Delimiter::Semicolon);
    assert_eq!(text(&screen.rows), [["a", "b,c"], ["1", "2,3"]]);
    wait_for_index(&document);
    let old_index = document.index_job();

    let comma = Choices {
        delimiter: Some(Delimiter::Comma),
        ..Choices::default()
    };
    let screen = document.reinterpret(comma, 10, 1000).unwrap();
    assert_eq!(screen.generation, 1);
    assert_eq!(screen.detection.delimiter, Delimiter::Comma);
    assert_eq!(text(&screen.rows), [["a;b", "c"], ["1;\"2", "3\""]]);
    wait_for_index(&document);
    assert_eq!(document.generation(), 1);
    assert_eq!(document.rows(0..2, 1000).unwrap(), screen.rows);
    assert_ne!(
        document.index_job().control().id(),
        old_index.control().id()
    );
    let generations: Vec<u64> = reports.try_iter().map(|p| p.generation).collect();
    assert_eq!(generations, [0, 1]);

    // A choice that doesn't fit leaves the document as it was.
    let utf16 = Choices {
        encoding: Some(Encoding::Utf16Le),
        ..Choices::default()
    };
    assert!(matches!(
        document.reinterpret(utf16, 10, 1000),
        Err(DocumentError::Choice(_))
    ));
    assert_eq!(document.generation(), 1);
    assert_eq!(document.detection().delimiter, Delimiter::Comma);
}

#[test]
fn reinterpreting_cancels_the_old_jobs() {
    let dir = Dir::new("reinterpret-cancel");
    let bytes = sample(64 * REVIEW_CHUNK_BYTES);
    let path = dir.file("big.csv", &bytes);
    let scheduler = scheduler();
    scheduler.set_interacting(true);
    let (document, _) = Document::open(
        &path,
        &dir.temp(),
        VolumeInfo::default(),
        &scheduler,
        options(5),
        None,
    )
    .unwrap();
    let old_review = document.review_job();
    let semicolon = Choices {
        delimiter: Some(Delimiter::Semicolon),
        ..Choices::default()
    };
    document.reinterpret(semicolon, 5, 100).unwrap();
    assert_eq!(
        old_review.control().wait_timeout(LONG),
        Some(Err(JobError::Cancelled))
    );
    scheduler.set_interacting(false);
    assert_eq!(
        document.review_job().control().wait_timeout(LONG),
        Some(Ok(()))
    );
}

// ---------------------------------------------------------------------------
// A file on a removable drive (ADR-0006)

fn open_removable(
    dir: &Dir,
    bytes: &[u8],
    chunk: usize,
    scheduler: &Scheduler,
) -> (Document, FirstScreen) {
    let path = dir.file("usb.csv", bytes);
    let source = Source::open_simulating_removable(&path, &dir.temp(), chunk).unwrap();
    assert_eq!(source.storage(), Storage::Reading);
    Document::from_source(source, scheduler, options(30), None).unwrap()
}

#[test]
fn a_removable_file_is_indexed_from_the_copy_pass() {
    let dir = Dir::new("removable");
    let bytes = sample(300 * 1024);
    let gate = Gate::closed();
    let scheduler = scheduler_with(Arc::clone(&gate));
    // Odd-sized chunks, so they cut rows, CRLFs and quotes.
    let (document, screen) = open_removable(&dir, &bytes, 4093, &scheduler);
    let parser = document.current().parser;
    let all = expected_rows(&bytes, parser, 1000);
    assert_eq!(screen.rows, all[..30]);
    // Before the copy: rows from the first 64 KB, read with ordinary reads.
    assert_eq!(document.storage(), Storage::Reading);
    assert_eq!(document.rows(10..20, 1000).unwrap(), all[10..20]);
    // The review waits for the copy, as it needs the whole file mapped.
    assert!(!document.review_job().is_finished());

    gate.open();
    assert_eq!(wait_for_index(&document).rows, all.len());
    assert_eq!(document.storage(), Storage::Copy);
    assert_eq!(document.rows(0..all.len(), 1000).unwrap(), all);
    assert_eq!(
        document.review_job().control().wait_timeout(LONG),
        Some(Ok(()))
    );
    assert_eq!(
        document.review_job().wait().unwrap().delimiter_suggestion,
        None
    );
}

// ---------------------------------------------------------------------------
// Diagnostics (task 1.5)

/// [`sample`] with every diagnostic kind but the unterminated quote and the
/// BOM, spread through the file so chunks cut them: invalid UTF-8 (one
/// sequence cut by most chunk boundaries), NULs, text after a closing
/// quote, ragged rows, blank lines and CRLFs among LFs.
fn messy_sample(size: usize) -> Vec<u8> {
    let mut bytes = b"id,name,notes\n".to_vec();
    let mut i = 0;
    while bytes.len() < size {
        let row: Vec<u8> = match i % 50 {
            7 => b"7,bad \xFF byte,x\n".to_vec(),
            13 => b"13,nul \0 here,\"q\"after\n".to_vec(),
            21 => b"21,short\n".to_vec(),
            29 => b"\n".to_vec(),
            37 => format!("{i},caf\u{e9} {i},crlf\r\n").into_bytes(),
            _ => format!("{i},caf\u{e9} {i},\u{1F600}{i}\n").into_bytes(),
        };
        bytes.extend_from_slice(&row);
        i += 1;
    }
    bytes
}

/// The diagnostics of `bytes` read as `document` reads them, from an index
/// of the whole slice: what the document's must equal.
fn reference_diagnostics(document: &Document, bytes: &[u8]) -> Arc<Diagnostics> {
    let detection = document.detection();
    let dialect = IndexDialect {
        delimiter: detection.delimiter.byte(),
        quote: QUOTE,
        code_unit: detection.encoding.code_unit(),
        bom_len: detection.bom.len(),
    };
    let (_, diagnostics, indexer) =
        RowIndex::start_with_diagnostics(dialect, detection.encoding).unwrap();
    indexer.run(bytes, &AtomicBool::new(false), |_| {}).unwrap();
    diagnostics
}

/// Checks that `document`'s diagnostics, once indexed, equal the
/// reference's: the report, and every row's mark and neighbours.
fn check_document_diagnostics(document: &Document, bytes: &[u8]) {
    let rows = wait_for_index(document).rows;
    let want = reference_diagnostics(document, bytes);
    let report = document.diagnostics();
    assert!(report.is_complete());
    assert_eq!(report.rows(), rows);
    assert_eq!(*report, *want.report());
    for kind in [
        DiagnosticKind::InvalidEncoding,
        DiagnosticKind::NulBytes,
        DiagnosticKind::TextAfterClosingQuote,
        DiagnosticKind::RaggedRows,
        DiagnosticKind::MixedLineEndings,
        DiagnosticKind::BlankLines,
    ] {
        assert!(report.get(kind).is_some(), "{kind:?} in {report:?}");
    }
    let mut marked = 0;
    for row in 0..rows {
        assert_eq!(
            document.row_has_diagnostic(row),
            want.row_has_diagnostic(row),
            "row {row}"
        );
        marked += usize::from(want.row_has_diagnostic(row));
    }
    assert!(marked > MAX_LOCATIONS, "only {marked} marked rows");
    for from in (0..=rows + 1).step_by(97) {
        assert_eq!(
            document.next_row_with_diagnostic(from),
            want.next_row_with_diagnostic(from)
        );
        assert_eq!(
            document.previous_row_with_diagnostic(from),
            want.previous_row_with_diagnostic(from)
        );
    }
}

#[test]
fn diagnostics_arrive_through_the_document() {
    let dir = Dir::new("diagnostics");
    // More than 1,000 occurrences of the commonest kinds, over many chunks.
    let bytes = messy_sample(3 * crate::index::DIAGNOSTICS_CHUNK_BYTES + 1000);
    let path = dir.file("messy.csv", &bytes);
    let (document, _) = Document::open(
        &path,
        &dir.temp(),
        VolumeInfo::default(),
        &scheduler(),
        options(5),
        None,
    )
    .unwrap();
    assert_eq!(document.detection().encoding, Encoding::Utf8);
    check_document_diagnostics(&document, &bytes);

    // Reading the file again starts its diagnostics again, for the new
    // reading: here, Windows-1252, where 0xFF is a valid byte.
    let choices = Choices {
        encoding: Some(Encoding::Windows1252),
        ..Choices::default()
    };
    document.reinterpret(choices, 5, 1000).unwrap();
    wait_for_index(&document);
    let report = document.diagnostics();
    assert!(report.is_complete());
    assert!(report.get(DiagnosticKind::InvalidEncoding).is_none());
    assert!(report.get(DiagnosticKind::NulBytes).is_some());
}

/// A removable drive's file is indexed from `Source::stream`'s chunks
/// (1.3a), and its diagnostics are collected chunk by chunk, with chunks
/// of an odd size so they cut multibyte characters, CRLFs and quotes.
#[test]
fn diagnostics_of_a_removable_file_arrive_chunk_by_chunk() {
    let dir = Dir::new("diagnostics-removable");
    let bytes = messy_sample(700 * 1024);
    let (document, _) = open_removable(&dir, &bytes, 4093, &scheduler());
    check_document_diagnostics(&document, &bytes);
    assert_eq!(document.storage(), Storage::Copy);
}

/// First paint does no diagnostics work (DESIGN §3.10 rule 1; "First-paint
/// regression" in `docs/tasks/1.5.md`): the index job makes the reading's
/// diagnostics when it starts. With its thread held at the start, the
/// document has none yet, and readers see an empty, incomplete report and
/// no marked rows. The same holds for a reading made by `reinterpret`.
#[test]
fn first_paint_makes_no_diagnostics() {
    let dir = Dir::new("first-paint-diagnostics");
    let bytes = messy_sample(2 * crate::index::DIAGNOSTICS_CHUNK_BYTES);
    let path = dir.file("messy.csv", &bytes);
    let gate = Gate::closed();
    let scheduler = scheduler_with(Arc::clone(&gate));
    let check_none_yet = |document: &Document| {
        assert!(
            document.current().diagnostics.get().is_none(),
            "first paint made the diagnostics"
        );
        assert_eq!(*document.diagnostics(), Report::default());
        assert_eq!(document.diagnostics_with_generation().1.rows(), 0);
        assert!(!document.row_has_diagnostic(0));
        assert_eq!(document.next_row_with_diagnostic(0), None);
        assert_eq!(document.previous_row_with_diagnostic(usize::MAX), None);
    };

    let (document, _) = Document::open(
        &path,
        &dir.temp(),
        VolumeInfo::default(),
        &scheduler,
        options(5),
        None,
    )
    .unwrap();
    check_none_yet(&document);
    gate.open();
    check_document_diagnostics(&document, &bytes);
    assert!(document.next_row_with_diagnostic(0).is_some());

    // A new reading starts with none either, not the old reading's.
    *gate.closed.lock().unwrap() = true;
    let choices = Choices {
        encoding: Some(Encoding::Windows1252),
        ..Choices::default()
    };
    document.reinterpret(choices, 5, 1000).unwrap();
    check_none_yet(&document);
    gate.open();
    wait_for_index(&document);
    assert!(document.diagnostics().is_complete());
}

/// On a removable drive, `Source::stream` checks the cancel flag only
/// between its 1 MiB chunks, but the index pushes each in 256 KiB pieces.
/// A cancel during the first piece stops the index after that piece, not
/// after the stream's chunk (1.5 integration review).
#[test]
fn cancelling_a_removable_index_stops_within_one_piece() {
    let dir = Dir::new("removable-cancel");
    let bytes = sample(3 * crate::source::STREAM_CHUNK_BYTES);
    let path = dir.file("usb.csv", &bytes);
    let source =
        Source::open_simulating_removable(&path, &dir.temp(), crate::source::STREAM_CHUNK_BYTES)
            .unwrap();
    let gate = Gate::closed();
    let scheduler = scheduler_with(Arc::clone(&gate));
    // The progress callback cancels the index job the first time it is
    // called, after the first piece of the first stream chunk.
    let job: Arc<Mutex<Option<JobHandle<IndexSummary>>>> = Arc::default();
    let reports = Arc::new(AtomicU64::new(0));
    let progress: ProgressCallback = {
        let (job, reports) = (Arc::clone(&job), Arc::clone(&reports));
        Arc::new(move |_| {
            reports.fetch_add(1, Ordering::SeqCst);
            if let Some(job) = job.lock().unwrap().as_ref() {
                job.cancel();
            }
        })
    };
    let (document, _) =
        Document::from_source(source, &scheduler, options(5), Some(progress)).unwrap();
    *job.lock().unwrap() = Some(document.index_job());
    gate.open();
    assert_eq!(
        document.index_job().control().wait_timeout(LONG),
        Some(Err(JobError::Cancelled))
    );
    assert_eq!(reports.load(Ordering::SeqCst), 1, "one piece, then stopped");
    let scanned = document.progress().bytes_scanned;
    assert!(
        scanned <= u64::try_from(crate::index::DIAGNOSTICS_CHUNK_BYTES).unwrap(),
        "scanned {scanned} bytes"
    );
}

#[test]
fn rows_of_a_removable_file_are_read_while_it_is_copied() {
    let dir = Dir::new("removable-progress");
    let bytes = sample(300 * 1024);
    // Held until the progress callback can reach the document.
    let gate = Gate::closed();
    let scheduler = scheduler_with(Arc::clone(&gate));
    let path = dir.file("usb.csv", &bytes);
    let source = Source::open_simulating_removable(&path, &dir.temp(), 8192).unwrap();
    let parser_rows = Arc::new(Mutex::new(Vec::new()));
    let document_slot: Arc<Mutex<Option<Arc<Document>>>> = Arc::default();
    let checked = Arc::new(AtomicBool::new(false));
    // From a progress report part-way through, read the newest rows: they
    // come from the drive (or the part already copied), not from a map.
    let progress: ProgressCallback = Arc::new({
        let (slot, rows, checked) = (
            Arc::clone(&document_slot),
            Arc::clone(&parser_rows),
            Arc::clone(&checked),
        );
        move |p: IndexProgress| {
            if p.complete || p.rows < 3000 || checked.load(Ordering::SeqCst) {
                return;
            }
            if let Some(document) = slot.lock().unwrap().as_ref() {
                assert_eq!(document.storage(), Storage::Reading);
                let read = document.rows(p.rows - 50..p.rows, 1000).unwrap();
                rows.lock().unwrap().push((p.rows, read));
                checked.store(true, Ordering::SeqCst);
            }
        }
    });
    let (document, _) =
        Document::from_source(source, &scheduler, options(5), Some(progress)).unwrap();
    let document = Arc::new(document);
    *document_slot.lock().unwrap() = Some(Arc::clone(&document));
    gate.open();
    wait_for_index(&document);
    let all = expected_rows(&bytes, document.current().parser, 1000);
    let seen = parser_rows.lock().unwrap();
    assert_eq!(seen.len(), 1, "rows were read part-way through the copy");
    let (rows, read) = &seen[0];
    assert!(*rows < all.len());
    assert_eq!(read, &all[rows - 50..*rows]);
    drop(seen);
    *document_slot.lock().unwrap() = None;
}

// ---------------------------------------------------------------------------
// Sharing

#[test]
fn documents_are_send_and_sync() {
    fn send_sync<T: Send + Sync>() {}
    send_sync::<Document>();
}

// ---------------------------------------------------------------------------
// What the grid reads (task 1.6): a window of columns, the column count and
// the numeric columns.

/// A file whose rows have different lengths: a header of 4, rows of 4, one
/// of 2 and one of 6, so the mode is 4.
const RAGGED: &[u8] = b"a,b,c,d\n1,x,2.5,\n2,y\n3,z,4,w\n4,v,5,u,extra,more\n5,t,6,s\n";

fn open_bytes(dir: &Dir, name: &str, bytes: &[u8]) -> (Document, FirstScreen) {
    let path = dir.file(name, bytes);
    Document::open(
        &path,
        &dir.temp(),
        VolumeInfo::default(),
        &scheduler(),
        options(10),
        None,
    )
    .unwrap()
}

#[test]
fn cells_reads_a_window_of_columns_with_each_rows_field_count() {
    let dir = Dir::new("cells");
    let (document, _) = open_bytes(&dir, "ragged.csv", RAGGED);
    let all = document.rows(0..10, 100).unwrap();
    let window = document.cells(1..5, 1..4, 100).unwrap();
    assert_eq!(
        window.iter().map(|r| r.field_count).collect::<Vec<_>>(),
        [4, 2, 4, 6]
    );
    // The cells are the rows' own, cut to the window; a short row has
    // fewer.
    for (offset, row) in window.iter().enumerate() {
        let full = &all[1 + offset];
        let expected: Vec<Cell> = full.iter().skip(1).take(3).cloned().collect();
        assert_eq!(row.cells, expected, "row {}", 1 + offset);
    }
    assert_eq!(
        window[1].cells,
        [Cell {
            text: "y".to_owned(),
            truncated: false
        }]
    );
    // Past the last field, past the last row, or an empty window.
    let past = document.cells(4..5, 6..9, 100).unwrap();
    assert!(past[0].cells.is_empty());
    assert_eq!(past[0].field_count, 6);
    assert!(document.cells(9..12, 0..4, 100).unwrap().is_empty());
    assert!(
        document
            .cells(1..3, 2..2, 100)
            .unwrap()
            .iter()
            .all(|r| r.cells.is_empty())
    );
    // Cells are cut to `max_chars` as for `rows`.
    let cut = document.cells(0..1, 0..1, 0).unwrap();
    assert_eq!(cut[0].cells[0].text, "");
    assert!(cut[0].cells[0].truncated);
}

#[test]
fn the_column_count_is_the_most_common_field_count() {
    let dir = Dir::new("columns");
    let (document, screen) = open_bytes(&dir, "ragged.csv", RAGGED);
    assert_eq!(screen.column_count, 4);
    assert_eq!(document.column_count(), 4);
    wait_for_index(&document);
    assert_eq!(document.column_count(), 4);

    let (document, screen) = open_bytes(&dir, "empty.csv", b"");
    assert_eq!(screen.column_count, 0);
    assert_eq!(document.column_count(), 0);
}

#[test]
fn the_column_count_comes_from_the_first_64_kb_until_the_index_passes_them() {
    let dir = Dir::new("columns-head");
    // The first 64 KB have rows of 2 fields; the rest, enough rows of 3 to
    // change the mode once the index has them.
    let mut bytes = b"a,b\n".to_vec();
    while bytes.len() < 70 * 1024 {
        bytes.extend_from_slice(b"1,2\n");
    }
    while bytes.len() < 400 * 1024 {
        bytes.extend_from_slice(b"1,2,3\n");
    }
    let path = dir.file("a.csv", &bytes);
    let gate = Gate::closed();
    let scheduler = scheduler_with(Arc::clone(&gate));
    let (document, screen) = Document::open(
        &path,
        &dir.temp(),
        VolumeInfo::default(),
        &scheduler,
        options(10),
        None,
    )
    .unwrap();
    assert_eq!(screen.column_count, 2);
    assert_eq!(document.column_count(), 2);
    gate.open();
    wait_for_index(&document);
    assert_eq!(document.column_count(), 3);
}

#[test]
fn numeric_columns_skip_the_header_and_empty_cells() {
    let dir = Dir::new("numeric");
    let bytes = b"id,qty,price,notes,when\n\
        A-1,40,29.90,,2025-01-03\n\
        A-2,,\"1,234.50\",gift,2025-01-04\n\
        A-3,3,4.10,,2025-01-05\n";
    let (document, _) = open_bytes(&dir, "orders.csv", bytes);
    assert!(document.detection().header);
    assert_eq!(
        document.numeric_columns(100).unwrap(),
        [false, true, true, false, false]
    );
    // Only the sample counts: here the first data row alone, whose notes
    // are empty.
    assert_eq!(
        document.numeric_columns(1).unwrap(),
        [false, true, true, false, false]
    );
    assert_eq!(document.numeric_columns(0).unwrap(), Vec::<bool>::new());
}

#[test]
fn numeric_columns_include_the_first_row_without_a_header() {
    let dir = Dir::new("numeric-no-header");
    let (document, _) = open_bytes(
        &dir,
        "readings.csv",
        b"1,S-01,18.2\n2,S-02,18.3\n3,S-03,x\n",
    );
    assert!(!document.detection().header);
    assert_eq!(document.numeric_columns(100).unwrap(), [true, false, false]);
    assert_eq!(document.numeric_columns(2).unwrap(), [true, false, true]);
}
