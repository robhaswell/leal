//! **Find** over the reference file (PLAN 1.8): a whole search, as a P2
//! job on the background pool, from the first data row to the last, for
//! queries that take each path through the search.
//!
//! - `find/no_match`: a query nothing matches. Only the raw-bytes search
//!   runs; no row is split into fields. The speed of light.
//! - `find/rare`: `Reykjavík`, the city of about 1 row in 32 (non-ASCII,
//!   case-insensitive).
//! - `find/common`: `deliver`, a word in about 1 row in 5's notes.
//! - `find/every_row`: `SKU-`, in every row: every row is split and every
//!   value checked, and every row is kept.
//! - `find/quote`: `wrote "`, which has a quote, so the raw bytes are
//!   searched for `wrote "` or `wrote ""` (the file has `""` there).
//! - `find/every_value`: `wrote \u{FFFD}`, which stands for invalid bytes,
//!   so the raw bytes can't be searched: every value of every row is
//!   decoded and checked, as for a file in another encoding.
//! - `find/screen_during_find`: one screenful of cells read the way the
//!   grid reads them (`Document::cells`, 60 rows × 12 columns, 128
//!   characters a cell), while `find/every_row`'s search runs again and
//!   again on the pool. DESIGN §3.9's 1 ms budget for a screen holds while
//!   a search runs: the search shares no lock with the grid's reads but
//!   the index's, which both only read.
//!
//! Nothing signals user input, so the searches never pause. After the
//! measurements it prints the longest stretch of work between two of a
//! search's checkpoints, which DESIGN §3.10 rule 3 wants under ~5 ms (in
//! the quietest of a few runs: it is wall-clock time, so a preempted chunk
//! looks longer on a busy machine).

use std::hint::black_box;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use criterion::{Criterion, criterion_group, criterion_main};
use leal_core::document::{Document, OpenOptions};
use leal_core::find::Query;
use leal_core::schedule::{Scheduler, SchedulerConfig};
use leal_core::source::{TempFolders, VolumeInfo};

mod common;

/// The queries, with the number of matching cells in the reference file
/// (checked, so the benchmark measures the search it says it does).
const QUERIES: &[(&str, &str)] = &[
    ("no_match", "qqzx"),
    ("rare", "Reykjavík"),
    ("common", "deliver"),
    ("every_row", "SKU-"),
    ("quote", "wrote \""),
    ("every_value", "wrote \u{FFFD}"),
];

/// The first row of the screen read during a search: the middle.
const FIRST_ROW: usize = 500_000;

fn find(c: &mut Criterion) {
    let path = common::reference_file();
    let len = std::fs::metadata(&path).expect("the reference file").len();
    let temp_root = std::env::temp_dir().join(format!("leal-bench-find-{}", std::process::id()));
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

    let mut group = c.benchmark_group("find");
    common::whole_file(&mut group, len);
    group.sample_size(20);
    let mut longest = Vec::new();
    for &(name, text) in QUERIES {
        let query = Query::new(text);
        let mut chunk = Duration::ZERO;
        let mut matches = 0;
        group.bench_function(name, |b| {
            b.iter(|| {
                let search = document.find(black_box(&query)).expect("a query");
                let summary = *search.job().wait().expect("the search");
                chunk = chunk.max(search.job().control().longest_chunk());
                matches = summary.matches;
            });
        });
        match name {
            "no_match" | "every_value" => assert_eq!(matches, 0),
            _ => assert!(matches > 1_000, "{name}: {matches} matches"),
        }
        longest.push((name, matches, chunk));
    }
    group.finish();

    // A screen of cells while a search runs, over and over, on the pool.
    let document = Arc::new(document);
    let stop = Arc::new(AtomicBool::new(false));
    let searching = std::thread::spawn({
        let document = Arc::clone(&document);
        let stop = Arc::clone(&stop);
        move || {
            let query = Query::new("SKU-");
            let mut searches = 0;
            while !stop.load(Ordering::Relaxed) {
                let search = document.find(&query).expect("a query");
                let _ = search.job().wait();
                searches += 1;
            }
            searches
        }
    });
    let mut group = c.benchmark_group("find");
    group.bench_function("screen_during_find", |b| {
        b.iter(|| {
            let rows = document
                .cells(black_box(FIRST_ROW)..FIRST_ROW + 60, 0..12, 128)
                .expect("reading");
            assert_eq!(rows.len(), 60);
            rows
        });
    });
    group.finish();
    stop.store(true, Ordering::Relaxed);
    let searches = searching.join().expect("the search thread");
    assert!(searches > 0, "no search ran during the screen benchmark");

    for (name, matches, chunk) in longest {
        println!(
            "find/{name}: {matches} matching cells, longest chunk {:.2} ms",
            chunk.as_secs_f64() * 1e3
        );
    }
    println!("find/screen_during_find: {searches} searches ran meanwhile");
    drop(document);
    let _ = std::fs::remove_dir_all(&temp_root);
}

criterion_group!(benches, find);
criterion_main!(benches);
