//! Column inserts and deletes (PLAN 2.4b) on the reference file: what the
//! column operations cost the readers, and what one costs with many edits.
//!
//! - `column_edits/screen`: one screenful of cells read the way the grid
//!   reads them (`Document::cells`, 60 rows × 12 columns, 128 characters a
//!   cell) from the middle of the file, with no column inserted or
//!   deleted: the rows path as it was, for comparison with the next one.
//! - `column_edits/screen_10_ops`: the same screen once 10 columns have
//!   been inserted or deleted (alternately), each row laid out from its
//!   field count's default layout.
//! - `column_edits/insert_column_100k_edits`: a column inserted and the
//!   insert undone, with 100,000 rows edited (a cell each, spread over the
//!   file): Insert Column on the main thread, which looks at every row's
//!   field count and at each edited row (DESIGN §1's 16 ms for an edit).
//! - `column_edits/delete_column_100k_edits`: the same for a delete.

use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};
use leal_bench::report::Side;
use leal_core::document::{Document, OpenOptions};
use leal_core::schedule::{Scheduler, SchedulerConfig};
use leal_core::source::{TempFolders, VolumeInfo};

// Only `reference_file` and `canary` are used here: these benchmarks take
// at most milliseconds, so they keep criterion's default sampling rather
// than `whole_file`'s.
#[allow(dead_code)]
mod common;

/// The first row of the screen: the middle of the file.
const FIRST_ROW: usize = 500_000;

/// Rows on one screen.
const SCREEN_ROWS: usize = 60;

/// Rows edited.
const EDITED: usize = 100_000;

fn column_edits(c: &mut Criterion) {
    let path = common::reference_file();
    let temp_root =
        std::env::temp_dir().join(format!("leal-bench-column-edits-{}", std::process::id()));
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

    common::canary(c, "column_edits", Side::Before);
    let mut group = c.benchmark_group("column_edits");
    group.bench_function("screen", |b| b.iter(read_screen));

    // Ten operations, alternately inserting and deleting.
    let mut made = Vec::new();
    for k in 0..10 {
        let command = if k % 2 == 0 {
            document.insert_column(k % 7, "inserted")
        } else {
            document.delete_column(k % 5)
        };
        made.push(command.expect("an operation").expect("a change"));
    }
    group.bench_function("screen_10_ops", |b| b.iter(read_screen));
    for command in made.iter().rev() {
        document.apply(&command.inverse()).expect("an undo");
    }

    // A cell edited in 100,000 rows, spread over the file.
    let rows = document.row_count();
    let cells: Vec<(usize, usize, &str)> = (0..EDITED)
        .map(|k| ((k * 9_973) % rows, k % 12, "edited"))
        .collect();
    document
        .set_cells(&cells)
        .expect("the edits")
        .expect("a change");
    group.bench_function("insert_column_100k_edits", |b| {
        b.iter(|| {
            let command = document
                .insert_column(black_box(3), "new")
                .expect("an insert")
                .expect("a change");
            document.apply(&command.inverse()).expect("an undo");
        });
    });
    group.bench_function("delete_column_100k_edits", |b| {
        b.iter(|| {
            let command = document
                .delete_column(black_box(3))
                .expect("a delete")
                .expect("a change");
            document.apply(&command.inverse()).expect("an undo");
        });
    });
    group.finish();
    common::canary(c, "column_edits", Side::After);
    drop(document);
    let _ = std::fs::remove_dir_all(&temp_root);
}

criterion_group!(benches, column_edits);
criterion_main!(benches);
