//! Each kind's **Previous** and **Next** in the details popover (task 1.7),
//! past the report's first 1,000 locations, on a wide file: about 175 MB,
//! 120,000 rows of 200 fields. Every row has text after a closing quote,
//! so every row is flagged in the row marks, and the other field-level
//! kinds (a NUL in every 37th row, invalid UTF-8 in every 53rd) must be
//! told apart from it. Every 41st row is short, so ragged.
//!
//! Each search starts at row 100,000, well past the 1,000th occurrence of
//! every kind, and asserts what it found. The 1.7 review measured one NUL
//! **Next** at 254 ms on a file like this, before the searches stopped
//! reading rows through the grid's row cache and NULs and invalid UTF-8
//! were found with one search of the bytes.

use std::hint::black_box;
use std::path::PathBuf;
use std::time::Duration;

use criterion::{Criterion, criterion_group, criterion_main};
use leal_core::diagnostics::DiagnosticKind;
use leal_core::document::{Document, OpenOptions};
use leal_core::schedule::{Scheduler, SchedulerConfig};
use leal_core::source::{TempFolders, VolumeInfo};

const ROWS: usize = 120_000;
const FIELDS: usize = 200;
const START: usize = 100_000;

/// The file, written once into the benchmark data folder.
fn wide_messy_file() -> PathBuf {
    let dir = leal_bench::reference::data_dir();
    std::fs::create_dir_all(&dir).expect("the bench data folder");
    let path = dir.join("navigate-wide-v2.csv");
    if path.exists() {
        return path;
    }
    let mut bytes = Vec::with_capacity(ROWS * FIELDS * 8);
    let header: Vec<String> = (0..FIELDS).map(|c| format!("col_{c}")).collect();
    bytes.extend_from_slice(header.join(",").as_bytes());
    bytes.push(b'\n');
    for row in 1..=ROWS {
        let fields = if row % 41 == 0 { FIELDS - 1 } else { FIELDS };
        for field in 0..fields {
            if field > 0 {
                bytes.push(b',');
            }
            match field {
                5 => bytes.extend_from_slice(b"\"q\"after"),
                // Valid UTF-8, so the file is read as UTF-8 (ADR-0003).
                10 => bytes.extend_from_slice("caf\u{e9}".as_bytes()),
                100 if row % 37 == 0 => bytes.extend_from_slice(b"n\0l"),
                150 if row % 53 == 0 => bytes.extend_from_slice(b"bad \xFF"),
                _ => bytes.extend_from_slice(format!("v{row}").as_bytes()),
            }
        }
        bytes.push(b'\n');
    }
    std::fs::write(&path, &bytes).expect("writing the file");
    path
}

fn navigate(c: &mut Criterion) {
    let path = wide_messy_file();
    let temp_root =
        std::env::temp_dir().join(format!("leal-bench-navigate-{}", std::process::id()));
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
    .expect("opening the file");
    document.index_job().wait().expect("indexing");
    let report = document.diagnostics();
    for kind in [
        DiagnosticKind::NulBytes,
        DiagnosticKind::InvalidEncoding,
        DiagnosticKind::RaggedRows,
    ] {
        let count = report.get(kind).expect("the kind is there").count();
        assert!(count > 2_000, "{kind:?}: {count}");
    }

    let mut group = c.benchmark_group("navigate");
    group
        .warm_up_time(Duration::from_secs(1))
        .measurement_time(Duration::from_secs(3));
    let cases = [
        ("next_nul_wide", DiagnosticKind::NulBytes, true, 37),
        ("previous_nul_wide", DiagnosticKind::NulBytes, false, 37),
        (
            "next_invalid_wide",
            DiagnosticKind::InvalidEncoding,
            true,
            53,
        ),
        (
            "previous_invalid_wide",
            DiagnosticKind::InvalidEncoding,
            false,
            53,
        ),
        ("next_ragged_wide", DiagnosticKind::RaggedRows, true, 41),
        (
            "previous_ragged_wide",
            DiagnosticKind::RaggedRows,
            false,
            41,
        ),
        (
            "next_quote_wide",
            DiagnosticKind::TextAfterClosingQuote,
            true,
            1,
        ),
    ];
    for (name, kind, forward, every) in cases {
        // The physical row of data row `n` is `n` (the header is row 0).
        let want = if forward {
            START.div_ceil(every) * every
        } else {
            (START - 1) / every * every
        };
        group.bench_function(name, |b| {
            b.iter(|| {
                let found = if forward {
                    document.next_with_kind(kind, black_box(START))
                } else {
                    document.previous_with_kind(kind, black_box(START))
                };
                let place = found.expect("reading").expect("an occurrence");
                assert_eq!(place.row, want, "{name}");
            });
        });
    }
    group.finish();
    drop(document);
    let _ = std::fs::remove_dir_all(&temp_root);
}

criterion_group!(benches, navigate);
criterion_main!(benches);
