//! The speed-of-light baseline (PLAN 1.2b): how fast this machine can read
//! the reference file and find its quote, CR and LF bytes, without parsing
//! anything. Nothing that reads or indexes the file can beat these numbers,
//! so later benchmarks (the row index, 1.3) are compared with them.
//!
//! These are also CI's noise canaries: their code doesn't change between
//! commits, so if they move, the machine moved (see `src/report.rs`).
//!
//! Both run with the file in the page cache (criterion's warm-up reads it),
//! which is the case for a file the user just opened. A cold read from disk
//! needs `sudo purge` between iterations and isn't measured here.

mod common;

use std::fs::File;
use std::hint::black_box;
use std::io::Read;
use std::path::Path;

use criterion::{Criterion, criterion_group, criterion_main};

/// Reads the whole file through a 1 MiB buffer and returns the bytes read.
fn read_sequentially(path: &Path, buffer: &mut [u8]) -> u64 {
    let mut file = File::open(path).expect("opening the reference file");
    let mut total = 0;
    loop {
        let read = file.read(buffer).expect("reading the reference file");
        if read == 0 {
            return total;
        }
        // Make the compiler assume the bytes are used.
        black_box(&buffer[..read]);
        total += read as u64;
    }
}

/// Counts the quote, CR and LF bytes: the bytes a quote-aware row index
/// must look at. The file is already in memory.
fn scan_specials(bytes: &[u8]) -> usize {
    memchr::memchr3_iter(b'"', b'\r', b'\n', bytes).count()
}

fn baseline(c: &mut Criterion) {
    let path = common::reference_file();
    let bytes = std::fs::read(&path).expect("reading the reference file");
    let len = bytes.len() as u64;

    let mut group = c.benchmark_group("baseline");
    common::whole_file(&mut group, len);

    let mut buffer = vec![0; 1 << 20];
    group.bench_function("sequential_read", |b| {
        b.iter(|| {
            let read = read_sequentially(black_box(&path), &mut buffer);
            assert_eq!(read, len);
            read
        });
    });

    group.bench_function("memchr3_scan", |b| {
        b.iter(|| scan_specials(black_box(&bytes)));
    });

    group.finish();
}

criterion_group!(benches, baseline);
criterion_main!(benches);
