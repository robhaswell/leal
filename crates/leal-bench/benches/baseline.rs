//! The speed-of-light baseline (PLAN 1.2b): how fast this machine can read
//! the reference file and find its quote, CR and LF bytes, without parsing
//! anything. Nothing that reads or indexes the file can beat these numbers,
//! so later benchmarks (the row index, 1.3) are compared with them.
//!
//! `memchr3_scan` is also CI's noise canary: its code doesn't change
//! between commits, so if it moves, the machine moved (see `src/report.rs`).
//! `sequential_read` is reported for information only. It is short and
//! depends on the page cache and the VM's I/O, so on a shared CI runner it
//! moves too much to be a canary. `just bench-compare` runs both a second
//! time after all the other benchmarks, with
//! `LEAL_BENCH_BASELINE_GROUP=baseline-late`, so that noise late in a run is
//! caught too. That second run is reported under its own group name,
//! `baseline-late/…`.
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

    let name = std::env::var("LEAL_BENCH_BASELINE_GROUP").unwrap_or_else(|_| "baseline".into());
    assert!(
        leal_bench::report::BASELINE_GROUPS.contains(&name.as_str()),
        "LEAL_BENCH_BASELINE_GROUP must be one of {:?}",
        leal_bench::report::BASELINE_GROUPS
    );
    let mut group = c.benchmark_group(name);
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
