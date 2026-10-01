//! The row index (PLAN 1.3): building the full index of the reference file,
//! which DESIGN §1 budgets at under 500 ms. Compare it with
//! `baseline/memchr3_scan`, which finds the same quote, CR and LF bytes
//! without doing anything with them.
//!
//! `index/build` runs on the calling thread. `index/run` goes through the
//! progressive path the app uses (`RowIndex::start`, then `Indexer::run`
//! with a cancel flag and a progress callback), still on this thread so the
//! two measure the same work.
//!
//! `index/build_diagnostics` and `index/run_diagnostics` (PLAN 1.5) do the
//! same while collecting diagnostics, which the app does: the difference is
//! what diagnostics cost. `index/run_diagnostics_nul_heavy` is a worst case
//! for them: UTF-16 text without a BOM, read as Windows-1252, so every
//! field holds NULs. `index/run_utf16` and `index/run_diagnostics_utf16`
//! index the same text as UTF-16 with a BOM, without and with diagnostics.
//! These three aren't budgeted (DESIGN §1's budget is for the reference
//! file); see `docs/tasks/1.5.md`.

mod common;

use std::hint::black_box;
use std::sync::atomic::AtomicBool;

use criterion::{Criterion, criterion_group, criterion_main};
use leal_core::diagnostics::DiagnosticKind;
use leal_core::dialect::Encoding;
use leal_core::index::{CodeUnit, IndexDialect, RowIndex};

/// The reference file's dialect: comma, `"`, UTF-8, no BOM.
const DIALECT: IndexDialect = IndexDialect {
    delimiter: b',',
    quote: b'"',
    code_unit: CodeUnit::Byte,
    bom_len: 0,
};

/// The header plus 1,000,000 data rows.
const ROWS: u64 = leal_bench::reference::ROWS + 1;

fn index(c: &mut Criterion) {
    let path = common::reference_file();
    let bytes = std::fs::read(&path).expect("reading the reference file");
    let len = bytes.len() as u64;

    let mut group = c.benchmark_group("index");
    common::whole_file(&mut group, len);

    group.bench_function("build", |b| {
        b.iter(|| {
            let index = RowIndex::build(black_box(&bytes), DIALECT).expect("indexing");
            assert_eq!(index.row_count() as u64, ROWS);
            index
        });
    });

    group.bench_function("run", |b| {
        b.iter(|| {
            let (index, indexer) = RowIndex::start(DIALECT).expect("a valid dialect");
            let cancel = AtomicBool::new(false);
            let mut chunks = 0;
            indexer
                .run(black_box(&bytes), &cancel, |_| chunks += 1)
                .expect("indexing");
            assert_eq!(index.row_count() as u64, ROWS);
            assert_eq!(
                index.field_count_mode(),
                Some(leal_bench::reference::COLUMNS)
            );
            chunks
        });
    });

    group.bench_function("build_diagnostics", |b| {
        b.iter(|| {
            let (index, report) =
                RowIndex::build_with_diagnostics(black_box(&bytes), DIALECT, Encoding::Utf8)
                    .expect("indexing");
            assert_eq!(index.row_count() as u64, ROWS);
            // The reference file is regular (`reference.rs`).
            assert!(report.diagnostics().is_empty());
            report
        });
    });

    group.bench_function("run_diagnostics", |b| {
        b.iter(|| {
            let (index, diagnostics, indexer) =
                RowIndex::start_with_diagnostics(DIALECT, Encoding::Utf8).expect("a valid dialect");
            let cancel = AtomicBool::new(false);
            let mut chunks = 0;
            indexer
                .run(black_box(&bytes), &cancel, |_| {
                    // As the UI will: read the latest report after each chunk.
                    black_box(diagnostics.report());
                    chunks += 1;
                })
                .expect("indexing");
            assert_eq!(index.row_count() as u64, ROWS);
            assert!(diagnostics.report().is_complete());
            chunks
        });
    });

    group.finish();

    // The first half of the reference file as UTF-16 LE (about the same
    // size). Without a BOM, read as Windows-1252 bytes, it is the worst
    // case for NULs.
    let text = std::str::from_utf8(&bytes[..bytes.len() / 2]).unwrap_or_else(|e| {
        std::str::from_utf8(&bytes[..e.valid_up_to()]).expect("a valid prefix")
    });
    let utf16: Vec<u8> = text.encode_utf16().flat_map(u16::to_le_bytes).collect();
    let mut group = c.benchmark_group("index");
    common::whole_file(&mut group, utf16.len() as u64);
    group.bench_function("run_diagnostics_nul_heavy", |b| {
        b.iter(|| {
            let (_, diagnostics, indexer) =
                RowIndex::start_with_diagnostics(DIALECT, Encoding::Windows1252)
                    .expect("a valid dialect");
            indexer
                .run(black_box(&utf16), &AtomicBool::new(false), |_| {})
                .expect("indexing");
            let report = diagnostics.report();
            let nul = report.get(DiagnosticKind::NulBytes).expect("NULs");
            assert!(nul.count() > 1_000_000);
            report
        });
    });
    let mut with_bom = vec![0xFF, 0xFE];
    with_bom.extend_from_slice(&utf16);
    let utf16_dialect = IndexDialect {
        code_unit: CodeUnit::Utf16Le,
        bom_len: 2,
        ..DIALECT
    };
    group.bench_function("run_utf16", |b| {
        b.iter(|| {
            let (index, indexer) = RowIndex::start(utf16_dialect).expect("a valid dialect");
            indexer
                .run(black_box(&with_bom), &AtomicBool::new(false), |_| {})
                .expect("indexing");
            index
        });
    });
    group.bench_function("run_diagnostics_utf16", |b| {
        b.iter(|| {
            let (_, diagnostics, indexer) =
                RowIndex::start_with_diagnostics(utf16_dialect, Encoding::Utf16Le)
                    .expect("a valid dialect");
            indexer
                .run(black_box(&with_bom), &AtomicBool::new(false), |_| {})
                .expect("indexing");
            let report = diagnostics.report();
            assert_eq!(report.diagnostics().len(), 1, "only the BOM: {report:?}");
            report
        });
    });
    group.finish();
}

/// One pathological 100 MB file for [`worst`].
struct Worst {
    name: &'static str,
    /// Repeated to fill the file.
    pattern: &'static [u8],
    encoding: Encoding,
}

/// The worst cases for diagnostics (1.5 review): files where every field
/// or every row is an occurrence, so the per-occurrence work dominates.
/// Each is benchmarked with diagnostics (`worst/<name>`) and without
/// (`worst/<name>_plain`). Not budgeted: DESIGN §1's budget is for the
/// reference file. Before benchmarking, each prints the longest chunk
/// (`DIAGNOSTICS_CHUNK_BYTES`) it took, for DESIGN §3.10 rule 3 (chunks of
/// at most about 5 ms).
fn worst(c: &mut Criterion) {
    const LEN: usize = 100_000_000;
    let cases = [
        Worst {
            name: "invalid_utf8",
            pattern: b"\xFF,",
            encoding: Encoding::Utf8,
        },
        Worst {
            name: "invalid_and_nul",
            pattern: b"\xFF\0,",
            encoding: Encoding::Utf8,
        },
        Worst {
            name: "unmapped_1253",
            pattern: b"\xAA,",
            encoding: Encoding::Windows1253,
        },
        Worst {
            name: "blank_lines",
            pattern: b"\n",
            encoding: Encoding::Utf8,
        },
    ];
    let mut group = c.benchmark_group("worst");
    common::whole_file(&mut group, LEN as u64);
    // Each iteration takes up to about a second.
    group.sample_size(10);
    for case in cases {
        let bytes: Vec<u8> = case.pattern.iter().copied().cycle().take(LEN).collect();
        let run = || {
            let (index, diagnostics, indexer) =
                RowIndex::start_with_diagnostics(DIALECT, case.encoding).expect("a valid dialect");
            let mut longest = std::time::Duration::ZERO;
            let mut last = std::time::Instant::now();
            indexer
                .run(black_box(&bytes), &AtomicBool::new(false), |_| {
                    longest = longest.max(last.elapsed());
                    last = std::time::Instant::now();
                })
                .expect("indexing");
            assert!(diagnostics.report().shows_banner() || case.name == "blank_lines");
            (index, longest)
        };
        // The longest chunk of the slower of two warm runs: the first run
        // after making the file also pays to fault in fresh memory.
        drop(run());
        let (index, first) = run();
        let (_, second) = run();
        let longest = first.max(second);
        eprintln!(
            "worst/{}: {} rows, longest chunk {:.2} ms",
            case.name,
            index.row_count(),
            longest.as_secs_f64() * 1e3
        );
        drop(index);
        group.bench_function(case.name, |b| b.iter(run));
        group.bench_function(format!("{}_plain", case.name), |b| {
            b.iter(|| {
                let (index, indexer) = RowIndex::start(DIALECT).expect("a valid dialect");
                indexer
                    .run(black_box(&bytes), &AtomicBool::new(false), |_| {})
                    .expect("indexing");
                index
            });
        });
    }
    group.finish();
}

criterion_group!(benches, index, worst);
criterion_main!(benches);
