//! Shared set-up for the benchmarks. Each benchmark file includes it with
//! `mod common;`.

use std::fs::File;
use std::hint::black_box;
use std::io::Read;
use std::path::PathBuf;
use std::time::Duration;

use criterion::measurement::WallTime;
use criterion::{BenchmarkGroup, Criterion, SamplingMode, Throughput};
use leal_bench::report::{self, Side};

/// The reference file's path, generating it first if needed (about a
/// second in release builds).
pub fn reference_file() -> PathBuf {
    leal_bench::reference::ensure().expect("generating the reference file")
}

/// Settings for benchmarks over a whole file, where one iteration takes
/// milliseconds or more: every sample runs the same number of iterations
/// (criterion's default, linear sampling, would run 5,050 of them), and
/// results are reported as throughput over `bytes`.
pub fn whole_file(group: &mut BenchmarkGroup<'_, WallTime>, bytes: u64) {
    group
        .sampling_mode(SamplingMode::Flat)
        .sample_size(50)
        .warm_up_time(Duration::from_secs(2))
        .measurement_time(Duration::from_secs(5))
        .throughput(Throughput::Bytes(bytes));
}

/// How much of the reference file a group canary scans: 32 MiB, more than
/// the runner's caches hold, so like the run-wide canary it reads memory,
/// but a third of the time per iteration, so a short run takes enough of
/// them.
const CANARY_BYTES: usize = 32 << 20;

/// Runs the noise canary for `group`, on its `side`: a short `memchr3`
/// scan, the same work as `baseline/memchr3_scan` (`benches/baseline.rs`)
/// over the first 32 MiB of the reference file, with about 1 s of
/// measurement. Call it just before a group's first benchmark and just
/// after its `finish()`, with the group's name.
///
/// Its id is `canary/<bench>.<group>.<side>`
/// ([`report::group_canary_id`]): `just bench-compare` runs it on both
/// sides, and `bench-report` judges each benchmark's attempt noisy if its
/// group's canaries moved (`src/report.rs`). The bench target's name in
/// the id tells a third attempt which benchmark binary to run.
pub fn canary(c: &mut Criterion, group: &str, side: Side) {
    // This module is compiled into each bench target, so this is that
    // target's name: `index` in `benches/index.rs`.
    let name = report::group_canary_name(env!("CARGO_CRATE_NAME"), group, side);
    let mut canaries = c.benchmark_group(report::GROUP_CANARY);
    canaries
        .sampling_mode(SamplingMode::Flat)
        .sample_size(10)
        .warm_up_time(Duration::from_millis(300))
        .measurement_time(Duration::from_secs(1))
        .throughput(Throughput::Bytes(CANARY_BYTES as u64));
    // Criterion calls the closure only if the filter selects the canary,
    // and calls it again for each sample, so the bytes are read once, on
    // the first call, and freed after the canary.
    let mut bytes: Option<Vec<u8>> = None;
    canaries.bench_function(name, |b| {
        let bytes = bytes.get_or_insert_with(canary_bytes);
        b.iter(|| memchr::memchr3_iter(b'"', b'\r', b'\n', black_box(bytes)).count());
    });
    canaries.finish();
}

/// The first [`CANARY_BYTES`] of the reference file.
fn canary_bytes() -> Vec<u8> {
    let mut bytes = Vec::with_capacity(CANARY_BYTES);
    File::open(reference_file())
        .expect("opening the reference file")
        .take(CANARY_BYTES as u64)
        .read_to_end(&mut bytes)
        .expect("reading the reference file");
    assert_eq!(bytes.len(), CANARY_BYTES, "a short reference file");
    bytes
}
