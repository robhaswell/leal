//! Writes the reference file (DESIGN §1). `just reference-file` runs it.
//!
//! ```text
//! leal-refgen [--rows N] [--seed N] [--out PATH]
//! ```
//!
//! With no options it writes the reference file itself (1M rows, the
//! standard seed) to `target/bench-data/`. `--rows` and `--seed` write
//! variants, for example `--rows 10000000` for a file of about 1 GB.

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Instant;

use leal_bench::reference;

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("error: {message}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let mut rows = reference::ROWS;
    let mut seed = reference::SEED;
    let mut out = None;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut value = || args.next().ok_or(format!("{arg} needs a value"));
        match arg.as_str() {
            "--rows" => rows = value()?.parse().map_err(|e| format!("--rows: {e}"))?,
            "--seed" => seed = value()?.parse().map_err(|e| format!("--seed: {e}"))?,
            "--out" => out = Some(PathBuf::from(value()?)),
            "-h" | "--help" => {
                println!("usage: leal-refgen [--rows N] [--seed N] [--out PATH]");
                return Ok(());
            }
            _ => return Err(format!("unknown argument `{arg}` (try --help)")),
        }
    }
    let path = out.unwrap_or_else(|| {
        if rows == reference::ROWS && seed == reference::SEED {
            reference::path()
        } else {
            reference::data_dir().join(format!(
                "reference-v{}-rows{rows}-seed{seed}.csv",
                reference::VERSION
            ))
        }
    });

    let start = Instant::now();
    let bytes = reference::generate(&path, rows, seed)
        .map_err(|e| format!("writing {}: {e}", path.display()))?;
    println!(
        "{}: {rows} rows, {bytes} bytes, in {:.2} s",
        path.display(),
        start.elapsed().as_secs_f64()
    );
    Ok(())
}
