//! Shared set-up for the benchmarks. Each benchmark file includes it with
//! `mod common;`.

use std::path::PathBuf;
use std::time::Duration;

use criterion::measurement::WallTime;
use criterion::{BenchmarkGroup, SamplingMode, Throughput};

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
