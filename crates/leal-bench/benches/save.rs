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
//!
//! The file is a copy of the reference file (a clone, on APFS) in a
//! temporary folder: the benchmark saves over it. Both are one group, with
//! its noise canaries before and after (`common::canary`); each sets its
//! own sampling, which criterion applies to the benchmarks added after.

use std::sync::Arc;
use std::time::{Duration, Instant};

use criterion::{Criterion, SamplingMode, criterion_group, criterion_main};
use leal_bench::report::Side;
use leal_core::document::{Document, OpenOptions};
use leal_core::save::{SaveKind, SaveRequest};
use leal_core::schedule::{Scheduler, SchedulerConfig};
use leal_core::source::{TempFolders, VolumeInfo};

mod common;

/// The edited row: the middle of the file.
const ROW: usize = 500_000;

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
                .save_to_writer(&mut std::io::sink())
                .expect("the write")
        });
    });
    group.finish();
    common::canary(c, "save", Side::After);
    drop(document);
    let _ = std::fs::remove_dir_all(&root);
}

criterion_group!(benches, save);
criterion_main!(benches);
