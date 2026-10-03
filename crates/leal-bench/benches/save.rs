//! Saving (PLAN 2.2) the reference file after one edit, against DESIGN §1's
//! "Save after one edit < 500 ms".
//!
//! - `save/one_edit`: one cell edited in the middle of the file, then Save
//!   over it, timed from the request to the job's end: the check before
//!   writing, the plan, writing 100 MB next to the file a chunk at a time,
//!   the metadata and attributes, `F_BARRIERFSYNC`, the swap, and the rebase (its snapshot, a clone, and
//!   the shifted index). Disk speed decides most of it, so it is gated on
//!   its budget only, not on change between commits (`report::BUDGET_ONLY`).
//! - `save/write_no_disk`: the same save's work without the disk: the
//!   edited rows' splices, the snapshot's bytes copied a chunk at a time,
//!   the first 64 KB kept and the whole-file encoding census, into a writer
//!   that keeps nothing (`Document::save_to_writer`, a test hook). It is
//!   compared between commits, so a slower writer is caught where the
//!   disk's noise would hide it.
//! - `save/rows_deleted` (PLAN 2.4c): 1,000 rows deleted, spread over the
//!   file (about 1,000 pieces in the piece list), then Save over it, timed
//!   as `one_edit`: the writer walks the piece list, one delete a row, and
//!   the rebase builds the new index from the plan. Budget-only, as
//!   `one_edit`. TODO(2.4b): `save/column_insert`, a save after a column
//!   insert, once columns can be inserted.
//! - `save/rows_deleted_no_disk`: the same save's work without the disk,
//!   compared between commits.
//! - `save/utf8_from_utf16` (PLAN 2.3): Save As UTF-8 of the reference
//!   file written as UTF-16 LE (about 200 MB, read-only in v1), to a new
//!   place, timed from the request to the job's end: every byte converted
//!   a chunk at a time, 100 MB written and flushed, and the rebase, which
//!   waits for the new file's index pass. Budget-only, as `one_edit`.
//! - `save/utf8_no_disk`: the same conversion into a writer that keeps
//!   nothing, compared between commits.
//! - `save/utf8_from_1252`: Save As UTF-8 from a single-byte encoding into
//!   a writer that keeps nothing, compared between commits: the reference
//!   file read as Windows-1252, so each of its non-ASCII bytes is looked up
//!   in the encoding's table and written as two or three bytes of UTF-8
//!   (the text reads as mojibake; the work is the same).
//!
//! The file is a copy of the reference file (a clone, on APFS) in a
//! temporary folder: the benchmark saves over it. Both are one group, with
//! its noise canaries before and after (`common::canary`); each sets its
//! own sampling, which criterion applies to the benchmarks added after.

use std::sync::Arc;
use std::time::{Duration, Instant};

use criterion::{Criterion, SamplingMode, criterion_group, criterion_main};
use leal_bench::report::Side;
use leal_core::detect::Choices;
use leal_core::dialect::Encoding;
use leal_core::document::{Document, OpenOptions};
use leal_core::save::{SaveKind, SaveRequest};
use leal_core::schedule::{Scheduler, SchedulerConfig};
use leal_core::source::{TempFolders, VolumeInfo};

mod common;

/// The edited row: the middle of the file.
const ROW: usize = 500_000;

/// Rows deleted for `save/rows_deleted`.
const DELETED: usize = 1_000;

/// Deletes [`DELETED`] rows of `document`, one at a time, spread over it
/// (not the header row).
fn delete_spread(document: &Document) {
    document.index_job().wait().expect("indexing");
    let step = document.row_count() / DELETED;
    for k in (0..DELETED).rev() {
        document
            .delete_rows(1 + k * step, 1)
            .expect("the delete")
            .expect("a change");
    }
}

fn save(c: &mut Criterion) {
    let reference = common::reference_file();
    let root = std::env::temp_dir().join(format!("leal-bench-save-{}", std::process::id()));
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
    let mut edits = 0_u64;

    common::canary(c, "save", Side::Before);
    let mut group = c.benchmark_group("save");
    group
        .sampling_mode(SamplingMode::Flat)
        .sample_size(10)
        .warm_up_time(Duration::from_secs(1))
        .measurement_time(Duration::from_secs(10));
    group.bench_function("one_edit", |b| {
        b.iter_custom(|iterations| {
            let mut total = Duration::ZERO;
            for _ in 0..iterations {
                document.index_job().wait().expect("indexing");
                edits += 1;
                document
                    .set_cell(ROW, 10, &format!("edited {edits}"))
                    .expect("the edit")
                    .expect("a change");
                let started = Instant::now();
                let job = document.save(SaveRequest::new(&path, SaveKind::Save));
                job.wait().expect("the save");
                total += started.elapsed();
            }
            total
        });
    });

    common::whole_file(&mut group, document.source().len());
    group.bench_function("write_no_disk", |b| {
        b.iter(|| {
            document
                .save_to_writer(SaveKind::SaveAs, &mut std::io::sink())
                .expect("the write")
        });
    });

    // A save after 1,000 rows deleted (task 2.4c): the file loses them
    // each time.
    group
        .sampling_mode(SamplingMode::Flat)
        .sample_size(10)
        .warm_up_time(Duration::from_secs(1))
        .measurement_time(Duration::from_secs(10));
    group.bench_function("rows_deleted", |b| {
        b.iter_custom(|iterations| {
            let mut total = Duration::ZERO;
            for _ in 0..iterations {
                delete_spread(&document);
                let started = Instant::now();
                let job = document.save(SaveRequest::new(&path, SaveKind::Save));
                job.wait().expect("the save");
                total += started.elapsed();
            }
            total
        });
    });

    delete_spread(&document);
    common::whole_file(&mut group, document.source().len());
    group.bench_function("rows_deleted_no_disk", |b| {
        b.iter(|| {
            document
                .save_to_writer(SaveKind::SaveAs, &mut std::io::sink())
                .expect("the write")
        });
    });

    // Save As UTF-8 (task 2.3) of the reference file written as UTF-16
    // (read-only in v1): every byte converted.
    let utf16 = root.join("reference-utf16.csv");
    write_utf16(&reference, &utf16);
    let open = |path: &std::path::Path| {
        let (document, _) = Document::open(
            path,
            &temp,
            VolumeInfo::default(),
            &scheduler,
            OpenOptions::default(),
            None,
        )
        .expect("opening the UTF-16 copy");
        document.index_job().wait().expect("indexing");
        Arc::new(document)
    };
    let utf16_document = open(&utf16);
    let utf16_len = utf16_document.source().len();
    common::whole_file(&mut group, utf16_len);
    group
        .sampling_mode(SamplingMode::Flat)
        .sample_size(10)
        .warm_up_time(Duration::from_secs(1))
        .measurement_time(Duration::from_secs(15));
    let mut saves = 0_u64;
    group.bench_function("utf8_from_utf16", |b| {
        b.iter_custom(|iterations| {
            let mut total = Duration::ZERO;
            for _ in 0..iterations {
                // A fresh document each time: the save makes it the UTF-8
                // file. Opening and indexing it aren't timed.
                let document = open(&utf16);
                saves += 1;
                let destination = root.join(format!("utf8-{saves}.csv"));
                let started = Instant::now();
                let job = document.save(SaveRequest::new(&destination, SaveKind::SaveAsUtf8));
                job.wait().expect("the save");
                total += started.elapsed();
                drop(document);
                let _ = std::fs::remove_file(&destination);
            }
            total
        });
    });

    common::whole_file(&mut group, utf16_len);
    group.bench_function("utf8_no_disk", |b| {
        b.iter(|| {
            utf16_document
                .save_to_writer(SaveKind::SaveAsUtf8, &mut std::io::sink())
                .expect("the write")
        });
    });

    // Save As UTF-8 from a single-byte encoding: a copy of the reference
    // file, read as Windows-1252.
    let single_byte = root.join("reference-1252.csv");
    std::fs::copy(&reference, &single_byte).expect("a copy of the reference file");
    let (windows_1252, _) = Document::open(
        &single_byte,
        &temp,
        VolumeInfo::default(),
        &scheduler,
        OpenOptions {
            choices: Choices {
                encoding: Some(Encoding::Windows1252),
                ..Choices::default()
            },
            ..OpenOptions::default()
        },
        None,
    )
    .expect("opening the copy as Windows-1252");
    windows_1252.index_job().wait().expect("indexing");
    common::whole_file(&mut group, windows_1252.source().len());
    group.bench_function("utf8_from_1252", |b| {
        b.iter(|| {
            windows_1252
                .save_to_writer(SaveKind::SaveAsUtf8, &mut std::io::sink())
                .expect("the write")
        });
    });
    group.finish();
    common::canary(c, "save", Side::After);
    drop(windows_1252);
    drop(utf16_document);
    drop(document);
    let _ = std::fs::remove_dir_all(&root);
}

/// Writes the reference file at `from` (UTF-8, no BOM) to `to` as UTF-16
/// LE with a BOM.
fn write_utf16(from: &std::path::Path, to: &std::path::Path) {
    use std::io::Write;
    let text = std::fs::read_to_string(from).expect("the reference file, in UTF-8");
    let mut out = std::io::BufWriter::new(std::fs::File::create(to).expect("the UTF-16 copy"));
    out.write_all(&[0xFF, 0xFE]).expect("writing");
    for unit in text.encode_utf16() {
        out.write_all(&unit.to_le_bytes()).expect("writing");
    }
    out.flush().expect("writing");
}

criterion_group!(benches, save);
criterion_main!(benches);
