//! Row inserts and deletes (PLAN 2.4a) on the reference file: what the
//! piece list costs the readers once there are many, and what one costs.
//!
//! - `row_edits/screen`: one screenful of cells read the way the grid reads
//!   them (`Document::cells`, 60 rows × 12 columns, 128 characters a cell)
//!   from the middle of the file, with no rows inserted or deleted: the
//!   rows path as it was, for comparison with the next one.
//! - `row_edits/screen_100k_row_edits`: the same screen once 100,000 rows
//!   have been inserted or deleted (50,000 of each, spread over the file),
//!   the screen's own rows among them, alternately deleted and inserted,
//!   so it is read as about 90 segments of the piece list, each a look-up
//!   in it (O(log P) for P pieces, about 200,000 here).
//! - `row_edits/insert_row_100k_row_edits`: one row inserted and the
//!   insert undone, with those edits in place: Insert Row on the main
//!   thread (DESIGN §1's 16 ms for an edit includes it).
//! - `row_edits/delete_1k_rows_100k_row_edits`: 1,000 rows deleted and the
//!   delete undone, which puts the same rows back.
//! - `row_edits/duplicate_10k_rows`: Duplicate Row's most rows at once
//!   (`DUPLICATE_ROW_LIMIT`, task 2.5a) copied from the middle of the file,
//!   before the 100,000 edits, and the copies undone: what Duplicate Row
//!   costs the main thread at worst.
//!
//! `undo_after_save` (phase 2 gate, `docs/tasks/2.G-a.md`), on a copy of
//! the reference file: a delete, saved (untimed), then undone, which after
//! a save works by value (ADR-0014 decision 3), on the main thread:
//!
//! - `undo_after_save/rows_10k` and `rows_100k`: 10,000 or 100,000 rows
//!   from the top of the file put back as inserted rows, their fields as
//!   their bytes, read from the snapshot the delete kept.
//! - `undo_after_save/column`: the second column put back in every row.

use std::hint::black_box;
use std::sync::Arc;
use std::time::{Duration, Instant};

use criterion::{Criterion, SamplingMode, criterion_group, criterion_main};
use leal_bench::report::Side;
use leal_core::document::{Document, OpenOptions};
use leal_core::edit::DUPLICATE_ROW_LIMIT;
use leal_core::save::{SaveKind, SaveRequest};
use leal_core::schedule::{Scheduler, SchedulerConfig};
use leal_core::source::{TempFolders, VolumeInfo};

// Only `reference_file` and `canary` are used here: these benchmarks take
// microseconds, so they keep criterion's default sampling rather than
// `whole_file`'s.
#[allow(dead_code)]
mod common;

/// The first row of the screen: the middle of the file.
const FIRST_ROW: usize = 500_000;

/// Rows on one screen.
const SCREEN_ROWS: usize = 60;

/// Rows inserted, and rows deleted.
const EACH: usize = 50_000;

fn row_edits(c: &mut Criterion) {
    let path = common::reference_file();
    let temp_root =
        std::env::temp_dir().join(format!("leal-bench-row-edits-{}", std::process::id()));
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

    common::canary(c, "row_edits", Side::Before);
    let mut group = c.benchmark_group("row_edits");
    group.bench_function("screen", |b| b.iter(read_screen));
    group.bench_function("duplicate_10k_rows", |b| {
        b.iter(|| {
            let command = document
                .duplicate_rows(black_box(FIRST_ROW), DUPLICATE_ROW_LIMIT)
                .expect("a duplicate")
                .expect("a change");
            document.apply(&command.inverse()).expect("an undo");
        });
    });

    // On screen: every other row deleted, and a row inserted after each
    // one left, from the bottom up so the places stay put.
    let inserted = vec![vec!["inserted".to_owned(); 12]];
    let mut made = 0;
    for k in (0..SCREEN_ROWS).rev() {
        let row = FIRST_ROW + 2 * k;
        document.delete_rows(row + 1, 1).expect("a delete");
        document.insert_rows(row + 1, &inserted).expect("an insert");
        made += 1;
    }
    // The rest spread over the file, off the screen.
    let rows = document.row_count();
    let mut k = 0;
    while made < EACH {
        // Below the screen, or past it (and before the last rows).
        let row = (k * 19_997) % (rows - 6 * SCREEN_ROWS);
        let row = if row >= FIRST_ROW - SCREEN_ROWS {
            row + 4 * SCREEN_ROWS
        } else {
            row
        };
        document.delete_rows(row, 1).expect("a delete");
        document
            .insert_rows(row + k % 7, &inserted)
            .expect("an insert");
        made += 1;
        k += 1;
    }
    assert_eq!(document.row_count(), rows);
    group.bench_function("screen_100k_row_edits", |b| {
        b.iter(|| {
            let rows = read_screen();
            assert_eq!(rows[1].cells[0].text, "inserted");
            rows
        });
    });
    let rows = vec![vec!["new".to_owned(); 12]];
    group.bench_function("insert_row_100k_row_edits", |b| {
        b.iter(|| {
            let command = document
                .insert_rows(black_box(FIRST_ROW + 7), &rows)
                .expect("an insert")
                .expect("a change");
            document.apply(&command.inverse()).expect("an undo");
        });
    });
    group.bench_function("delete_1k_rows_100k_row_edits", |b| {
        b.iter(|| {
            let command = document
                .delete_rows(black_box(FIRST_ROW - 2_000), 1_000)
                .expect("a delete")
                .expect("a change");
            document.apply(&command.inverse()).expect("an undo");
        });
    });
    group.finish();
    common::canary(c, "row_edits", Side::After);
    drop(document);
    let _ = std::fs::remove_dir_all(&temp_root);
}

/// A delete saved, then undone by value (`undo_after_save/…`).
fn undo_after_save(c: &mut Criterion) {
    let reference = common::reference_file();
    let root = std::env::temp_dir().join(format!("leal-bench-undo-{}", std::process::id()));
    std::fs::create_dir_all(&root).expect("a temporary folder");
    let path = root.join("reference.csv");
    std::fs::copy(&reference, &path).expect("a copy of the reference file");
    let temp = TempFolders::new(root.join("scratch"), root.join("records"));
    let scheduler = Scheduler::new(SchedulerConfig::default()).expect("a scheduler");
    let (document, _) = Document::open(
        &path,
        &temp,
        VolumeInfo::default(),
        &scheduler,
        OpenOptions::default(),
        None,
    )
    .expect("opening the copy");
    let document = Arc::new(document);
    let ready = || {
        document.index_job().wait().expect("indexing");
        document.review_job().wait().expect("the review");
    };
    ready();
    let rows = document.row_count();
    let save = || {
        document
            .save(SaveRequest::new(&path, SaveKind::Save))
            .wait()
            .expect("the save");
        ready();
    };
    // Each iteration makes its delete and saves it, untimed, then times
    // the undo, and saves again, untimed: the rows put back are the
    // file's own again, so the next delete takes rows of the file, not
    // inserted ones (whose undo needn't read the file).
    let undone = |iterations: u64, delete: &dyn Fn() -> leal_core::edit::Command| {
        let mut total = Duration::ZERO;
        for _ in 0..iterations {
            let command = delete();
            save();
            let started = Instant::now();
            document.apply(&command.inverse()).expect("the undo");
            total += started.elapsed();
            assert_eq!(document.row_count(), rows);
            save();
        }
        total
    };

    common::canary(c, "undo_after_save", Side::Before);
    let mut group = c.benchmark_group("undo_after_save");
    group
        .sampling_mode(SamplingMode::Flat)
        .sample_size(10)
        .warm_up_time(Duration::from_secs(1))
        .measurement_time(Duration::from_secs(10));
    for (name, count) in [("rows_10k", 10_000), ("rows_100k", 100_000)] {
        group.bench_function(name, |b| {
            b.iter_custom(|iterations| {
                undone(iterations, &|| {
                    document
                        .delete_rows(black_box(1), count)
                        .expect("a delete")
                        .expect("a change")
                })
            });
        });
    }
    group.bench_function("column", |b| {
        b.iter_custom(|iterations| {
            undone(iterations, &|| {
                document
                    .delete_column(black_box(1))
                    .expect("a delete")
                    .expect("a change")
            })
        });
    });
    group.finish();
    common::canary(c, "undo_after_save", Side::After);
    drop(document);
    let _ = std::fs::remove_dir_all(&root);
}

criterion_group!(benches, row_edits, undo_after_save);
criterion_main!(benches);
