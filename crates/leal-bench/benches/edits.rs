//! Edits (PLAN 2.1) on the reference file: what the overlay costs the
//! readers once there are many edits, and what one edit costs.
//!
//! - `edits/screen`: one screenful of cells read the way the grid reads
//!   them (`Document::cells`, 60 rows × 12 columns, 128 characters a cell)
//!   from the middle of the file, with no edits: the grid's read through
//!   the document, for comparison with the next one.
//! - `edits/screen_100k_edited_rows`: the same screen once 100,000 rows of
//!   the file have an edited cell, every row on screen among them. Each row
//!   read looks itself up in the overlay (O(log n) in the edited rows) and
//!   is read as it reads now.
//! - `edits/set_cell_100k_edited_rows`: one edit and its undo, with those
//!   100,000 edits in place: what Return in the in-cell editor costs the
//!   main thread (DESIGN §1's "Cell edit to screen < 16 ms" includes it).
//! - `edits/set_cells_10k`: a batch of 10,000 cells (a paste of 1,000 rows
//!   by 10 columns) and its undo, with those 100,000 edits in place.
//! - `edits/paste_100k_cells` and `edits/clear_100k_cells`: Paste and
//!   Clear (task 2.6) at their most cells at once (`CELL_BATCH_LIMIT`):
//!   a block of 10,000 rows by 10 columns of tab-separated text pasted, or
//!   the same cells cleared, from the middle of the file, and the undo,
//!   with those 100,000 edits in place. What Paste and Delete cost the
//!   main thread in the core at worst.
//! - `edits/find_every_row_100k_edited_rows`: `find/every_row`'s search
//!   (`SKU-`, in every row) with those edits: an edited row is checked
//!   through the raw-byte search of its own bytes, then cell by cell.
//!
//! After the measurements it prints two timings criterion can't take:
//!
//! - **An edit and its undo while a search runs** (`find/every_row`'s,
//!   over and over), p50 and p99 of 5,000: the search must never make the
//!   overlay be copied, which with 100,000 edited rows would take a
//!   millisecond or more.
//! - **`Search::progress` after a 10,000-cell paste** into a finished
//!   search with over a million matching rows: the call must hand the
//!   recount to a job, not do it itself; then how long the job takes.

use std::hint::black_box;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use criterion::{Criterion, criterion_group, criterion_main};
use leal_bench::report::Side;
use leal_core::document::{Document, OpenOptions};
use leal_core::edit::CELL_BATCH_LIMIT;
use leal_core::find::Query;
use leal_core::schedule::{Scheduler, SchedulerConfig};
use leal_core::source::{TempFolders, VolumeInfo};

mod common;

/// The first row of the screen: the middle of the file.
const FIRST_ROW: usize = 500_000;

/// Rows on one screen.
const SCREEN_ROWS: usize = 60;

/// Edited rows: one in ten of the file's million.
const EDITED_ROWS: usize = 100_000;

fn edits(c: &mut Criterion) {
    let path = common::reference_file();
    let len = std::fs::metadata(&path).expect("the reference file").len();
    let temp_root = std::env::temp_dir().join(format!("leal-bench-edits-{}", std::process::id()));
    let temp = TempFolders::new(temp_root.join("scratch"), temp_root.join("records"));
    let scheduler = Scheduler::new(SchedulerConfig::default()).expect("a scheduler");
    let (document, _) = Document::open(
        &path,
        &temp,
        VolumeInfo::default(),
        &scheduler,
        OpenOptions::default(),
        None,
    )
    .expect("opening the reference file");
    document.index_job().wait().expect("indexing");
    document.review_job().wait().expect("the review");
    let screen = FIRST_ROW..FIRST_ROW + SCREEN_ROWS;
    let read_screen = || {
        let rows = document
            .cells(black_box(screen.clone()), 0..12, 128)
            .expect("reading");
        assert_eq!(rows.len(), SCREEN_ROWS);
        rows
    };

    common::canary(c, "edits", Side::Before);
    let mut group = c.benchmark_group("edits");
    group.bench_function("screen", |b| b.iter(read_screen));

    // Every row on screen, and the rest spread over the file.
    let rows = document.row_count();
    let spread = (rows - 1) / (EDITED_ROWS - SCREEN_ROWS);
    let edited: Vec<usize> = screen
        .clone()
        .chain(
            (1..rows)
                .step_by(spread)
                .filter(|row| !screen.contains(row)),
        )
        .take(EDITED_ROWS)
        .collect();
    assert_eq!(edited.len(), EDITED_ROWS);
    for &row in &edited {
        document
            .set_cell(row, 2, "edited SKU-")
            .expect("an edit")
            .expect("a change");
    }
    assert_eq!(document.edited_cells(), EDITED_ROWS);
    group.bench_function("screen_100k_edited_rows", |b| {
        b.iter(|| {
            let rows = read_screen();
            assert_eq!(rows[0].cells[2].text, "edited SKU-");
            rows
        });
    });
    group.bench_function("set_cell_100k_edited_rows", |b| {
        b.iter(|| {
            let command = document
                .set_cell(black_box(FIRST_ROW + 1), 3, "x")
                .expect("an edit")
                .expect("a change");
            document.apply(&command.inverse()).expect("an undo");
        });
    });
    // A paste of 1,000 rows by 10 columns, past the edited rows' columns.
    let paste_values: Vec<(usize, usize, String)> = (0..1_000)
        .flat_map(|r| (0..10).map(move |c| (FIRST_ROW + 100 + r, c, format!("v{r}.{c}"))))
        .collect();
    let paste: Vec<(usize, usize, &str)> = paste_values
        .iter()
        .map(|(r, c, v)| (*r, *c, v.as_str()))
        .collect();
    group.bench_function("set_cells_10k", |b| {
        b.iter(|| {
            let command = document
                .set_cells(black_box(&paste))
                .expect("a batch")
                .expect("a change");
            document.apply(&command.inverse()).expect("an undo");
        });
    });
    group.finish();

    let mut group = c.benchmark_group("edits");
    group.sample_size(20);
    let batch_rows = CELL_BATCH_LIMIT / 10;
    let block: Vec<String> = (0..batch_rows)
        .map(|r| {
            (0..10)
                .map(|c| format!("p{r}.{c}"))
                .collect::<Vec<_>>()
                .join("\t")
        })
        .collect();
    let block = block.join("\n");
    let area = FIRST_ROW - batch_rows / 2..FIRST_ROW + batch_rows / 2;
    group.bench_function("paste_100k_cells", |b| {
        b.iter(|| {
            let command = document
                .paste(black_box(area.clone()), 0..10, 12, &block)
                .expect("a paste")
                .expect("a change");
            document.apply(&command.inverse()).expect("an undo");
        });
    });
    group.bench_function("clear_100k_cells", |b| {
        b.iter(|| {
            let command = document
                .clear_cells(black_box(area.clone()), 0..10)
                .expect("a clear")
                .expect("a change");
            document.apply(&command.inverse()).expect("an undo");
        });
    });
    group.finish();

    let document = Arc::new(document);
    let (p50, p99) = edits_during_find(&document);
    println!(
        "edits/set_cell_during_find_100k_edited_rows: p50 {:.2} µs, p99 {:.2} µs",
        p50.as_secs_f64() * 1e6,
        p99.as_secs_f64() * 1e6
    );
    let (call, catch_up) = progress_after_a_paste(&document, &paste);
    println!(
        "edits/progress_after_10k_cells: {:.1} µs, then caught up in {:.1} ms",
        call.as_secs_f64() * 1e6,
        catch_up.as_secs_f64() * 1e3
    );

    let mut group = c.benchmark_group("edits");
    common::whole_file(&mut group, len);
    group.sample_size(20);
    let query = Query::new("SKU-");
    group.bench_function("find_every_row_100k_edited_rows", |b| {
        b.iter(|| {
            let search = document.find(black_box(&query)).expect("a query");
            let summary = *search.job().wait().expect("the search");
            assert!(summary.matches > 1_000_000, "{} matches", summary.matches);
            summary
        });
    });
    group.finish();
    common::canary(c, "edits", Side::After);
    drop(document);
    let _ = std::fs::remove_dir_all(&temp_root);
}

/// The p50 and p99 of 5,000 edits and undos made while a search runs over
/// and over on the pool.
fn edits_during_find(document: &Arc<Document>) -> (Duration, Duration) {
    let stop = Arc::new(AtomicBool::new(false));
    let searching = std::thread::spawn({
        let document = Arc::clone(document);
        let stop = Arc::clone(&stop);
        move || {
            let query = Query::new("SKU-");
            while !stop.load(Ordering::Relaxed) {
                let search = document.find(&query).expect("a query");
                let _ = search.job().wait();
            }
        }
    });
    let mut times: Vec<Duration> = (0..5_000)
        .map(|i| {
            let started = Instant::now();
            let command = document
                .set_cell(FIRST_ROW + 2 + i % 50, 4, "x")
                .expect("an edit")
                .expect("a change");
            document.apply(&command.inverse()).expect("an undo");
            started.elapsed()
        })
        .collect();
    stop.store(true, Ordering::Relaxed);
    searching.join().expect("the search thread");
    times.sort_unstable();
    (times[times.len() / 2], times[times.len() * 99 / 100])
}

/// How long `progress` takes just after a paste of `paste` into a finished
/// search that matches every row, and how long until the counts are caught
/// up. The paste is undone afterwards.
fn progress_after_a_paste(
    document: &Document,
    paste: &[(usize, usize, &str)],
) -> (Duration, Duration) {
    let every_row = document.find(&Query::new("SKU-")).expect("a query");
    every_row.job().wait().expect("the search");
    assert!(every_row.progress().matches > 1_000_000);
    let command = document
        .set_cells(paste)
        .expect("a batch")
        .expect("a change");
    let started = Instant::now();
    let progress = every_row.progress();
    let call = started.elapsed();
    assert!(progress.catching_up, "left to a job");
    while every_row.progress().catching_up {
        std::thread::sleep(Duration::from_micros(200));
    }
    let catch_up = started.elapsed();
    document.apply(&command.inverse()).expect("an undo");
    (call, catch_up)
}

criterion_group!(benches, edits);
criterion_main!(benches);
