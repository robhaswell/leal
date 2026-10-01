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
    // size), after a BOM. Without the BOM, read as Windows-1252 bytes, it is
    // the worst case for NULs. One buffer serves both, and the reference
    // file's bytes are freed first, so this binary holds at most about
    // 200 MB at once (the "First-paint regression" in `docs/tasks/1.5.md`).
    let with_bom = utf16_with_bom(&bytes);
    drop(bytes);
    let utf16 = &with_bom[2..];
    let mut group = c.benchmark_group("index");
    common::whole_file(&mut group, utf16.len() as u64);
    group.bench_function("run_diagnostics_nul_heavy", |b| {
        b.iter(|| {
            let (_, diagnostics, indexer) =
                RowIndex::start_with_diagnostics(DIALECT, Encoding::Windows1252)
                    .expect("a valid dialect");
            indexer
                .run(black_box(utf16), &AtomicBool::new(false), |_| {})
                .expect("indexing");
            let report = diagnostics.report();
            let nul = report.get(DiagnosticKind::NulBytes).expect("NULs");
            assert!(nul.count() > 1_000_000);
            report
        });
    });
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

/// A BOM (FF FE), then the first half of `bytes` (cut back to whole UTF-8
/// characters) as UTF-16 LE. Each UTF-8 byte gives at most one UTF-16 code
/// unit, so the buffer is allocated once, at its final size or a little
/// more.
fn utf16_with_bom(bytes: &[u8]) -> Vec<u8> {
    let half = &bytes[..bytes.len() / 2];
    let text = std::str::from_utf8(half)
        .unwrap_or_else(|e| std::str::from_utf8(&half[..e.valid_up_to()]).expect("a valid prefix"));
    let mut out = Vec::with_capacity(2 + 2 * text.len());
    out.extend_from_slice(&[0xFF, 0xFE]);
    out.extend(text.encode_utf16().flat_map(u16::to_le_bytes));
    out
}

/// One pathological file for [`worst`].
struct Worst {
    name: &'static str,
    /// Repeated to fill the file.
    pattern: &'static [u8],
    /// The file's length.
    len: usize,
    encoding: Encoding,
}

/// The worst cases for diagnostics (1.5 review): files where every field
/// or every row is an occurrence, so the per-occurrence work dominates.
/// Each is benchmarked with diagnostics (`worst/<name>`) and without
/// (`worst/<name>_plain`). Not budgeted: DESIGN §1's budget is for the
/// reference file. Before benchmarking, each prints the longest chunk
/// (`DIAGNOSTICS_CHUNK_BYTES`) it took, for DESIGN §3.10 rule 3 (chunks of
/// at most about 5 ms).
///
/// The files are small, and the cost is per byte, so compare throughput,
/// not time, with the 100 MB files these once were. Big files here pushed
/// CI's 7 GB runner into memory pressure just before the `open` benchmarks
/// ran ("First-paint regression" in `docs/tasks/1.5.md`). A file of blank
/// lines has as many rows as bytes. macOS's allocator keeps large freed
/// blocks counted against the process until the system needs the memory.
/// So an index of 100M rows (400 MB), made and dropped on every
/// iteration, added that much each time, up to several GB. With 1M rows
/// nothing builds up; with 2M, diagnostics' buffers already build up to
/// about 500 MB. The field-level cases are a single row, so only their own
/// bytes count: 25 MB each.
fn worst(c: &mut Criterion) {
    let cases = [
        Worst {
            name: "invalid_utf8",
            pattern: b"\xFF,",
            len: 25_000_000,
            encoding: Encoding::Utf8,
        },
        Worst {
            name: "invalid_and_nul",
            pattern: b"\xFF\0,",
            len: 25_000_000,
            encoding: Encoding::Utf8,
        },
        Worst {
            name: "unmapped_1253",
            pattern: b"\xAA,",
            len: 25_000_000,
            encoding: Encoding::Windows1253,
        },
        Worst {
            name: "blank_lines",
            pattern: b"\n",
            len: 1_000_000,
            encoding: Encoding::Utf8,
        },
    ];
    let mut group = c.benchmark_group("worst");
    for case in cases {
        common::whole_file(&mut group, case.len as u64);
        // Made at its final size, with no doubling on the way.
        let mut bytes = case.pattern.repeat(case.len.div_ceil(case.pattern.len()));
        bytes.truncate(case.len);
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
        // after making the file also pays to fault in fresh memory. Only
        // one index is alive at a time.
        drop(run());
        let (rows, first) = {
            let (index, longest) = run();
            (index.row_count(), longest)
        };
        let second = run().1;
        let longest = first.max(second);
        eprintln!(
            "worst/{}: {} bytes, {rows} rows, longest chunk {:.2} ms",
            case.name,
            case.len,
            longest.as_secs_f64() * 1e3
        );
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

/// **Next** and **Previous** over the row marks (1.5), which run on the
/// main thread: 400,000 rows with no warning, so each search reads every
/// row. Narrow rows (3 fields) are checked from their code alone, 64 at a
/// time; wide rows (130 fields, more than a code can hold) also need their
/// exact count from the side list, walked alongside.
fn marks(c: &mut Criterion) {
    const ROWS: usize = 400_000;
    let mut group = c.benchmark_group("marks");
    for (name, fields) in [("narrow", 3), ("wide", 130)] {
        let row = format!("{}\n", vec!["a"; fields].join(","));
        let bytes = row.repeat(ROWS).into_bytes();
        let (index, diagnostics, indexer) =
            RowIndex::start_with_diagnostics(DIALECT, Encoding::Utf8).expect("a valid dialect");
        indexer
            .run(&bytes, &AtomicBool::new(false), |_| {})
            .expect("indexing");
        assert_eq!(index.row_count(), ROWS);
        assert!(!diagnostics.report().shows_banner());
        // Only the marks are searched: the file (100 MB for the wide rows)
        // and the index can go.
        drop((index, bytes));
        group.bench_function(format!("next_{name}"), |b| {
            b.iter(|| {
                let found = diagnostics.next_row_with_diagnostic(black_box(0));
                assert_eq!(found, None);
            });
        });
        group.bench_function(format!("previous_{name}"), |b| {
            b.iter(|| {
                let found = diagnostics.previous_row_with_diagnostic(black_box(ROWS));
                assert_eq!(found, None);
            });
        });
    }
    group.finish();
}

criterion_group!(benches, index, worst, marks);
criterion_main!(benches);
