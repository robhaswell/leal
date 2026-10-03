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

use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};
use leal_bench::report::Side;
use leal_core::document::{Document, OpenOptions};
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

criterion_group!(benches, row_edits);
criterion_main!(benches);
