//! The row index (PLAN 1.3): building the full index of the reference file,
//! which DESIGN §1 budgets at under 500 ms. Compare it with
//! `baseline/memchr3_scan`, which finds the same quote, CR and LF bytes
//! without doing anything with them.
//!
//! `index/build` runs on the calling thread. `index/run` goes through the
//! progressive path the app uses (`RowIndex::start`, then `Indexer::run`
//! with a cancel flag and a progress callback), still on this thread so the
//! two measure the same work.

mod common;

use std::hint::black_box;
use std::sync::atomic::AtomicBool;

use criterion::{Criterion, criterion_group, criterion_main};
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

    group.finish();
}

criterion_group!(benches, index);
criterion_main!(benches);
