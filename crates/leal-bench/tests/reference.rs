//! The reference file generator: reproducible, and shaped as DESIGN §1
//! describes. These run on small row counts, so `just check` never writes
//! the full 100 MB; `just reference-file` checks the full file's SHA-256.

use std::path::PathBuf;

use leal_bench::reference::{self, COLUMNS, HEADER, ROWS, SEED};

fn generate(rows: u64, seed: u64) -> Vec<u8> {
    let mut out = Vec::new();
    let written = reference::write(&mut out, rows, seed).unwrap();
    assert_eq!(written, out.len() as u64, "write() returns the byte count");
    out
}

/// FNV-1a, 64-bit: a fixed, dependency-free hash for pinning output.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// Splits a whole file into records of fields, following RFC 4180 quoting
/// (the generator writes nothing irregular). Returns the raw field bytes,
/// quotes included, and panics on anything malformed.
fn records(bytes: &[u8]) -> Vec<Vec<&[u8]>> {
    let mut records = Vec::new();
    let mut fields = Vec::new();
    let mut start = 0;
    let mut in_quotes = false;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'"' if in_quotes && bytes.get(i + 1) == Some(&b'"') => i += 1,
            b'"' if in_quotes => {
                in_quotes = false;
                let next = bytes.get(i + 1).copied();
                assert!(
                    matches!(next, Some(b',' | b'\n')),
                    "closing quote at {i} is followed by {next:?}"
                );
            }
            b'"' => {
                assert_eq!(start, i, "quote in the middle of a field at {i}");
                in_quotes = true;
            }
            b',' if !in_quotes => {
                fields.push(&bytes[start..i]);
                start = i + 1;
            }
            b'\n' if !in_quotes => {
                fields.push(&bytes[start..i]);
                records.push(std::mem::take(&mut fields));
                start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    assert!(!in_quotes, "file ends inside a quoted field");
    assert_eq!(start, bytes.len(), "the last record has no line ending");
    records
}

#[test]
fn same_seed_gives_the_same_bytes() {
    assert_eq!(generate(2_000, SEED), generate(2_000, SEED));
}

#[test]
fn a_different_seed_gives_different_bytes() {
    assert_ne!(generate(2_000, SEED), generate(2_000, SEED + 1));
}

#[test]
fn fewer_rows_give_a_prefix_of_the_file() {
    let short = generate(500, SEED);
    let long = generate(1_000, SEED);
    assert!(long.starts_with(&short));
}

/// Pins the generator's output. If this fails, the reference file has
/// changed: bump `reference::VERSION` (so a cached file under the old name
/// is never reused), update this hash and the SHA-256 in
/// `crates/leal-bench/reference.sha256` (`just reference-file` prints it),
/// and say so in the task notes, since earlier benchmark numbers no longer
/// compare.
#[test]
fn output_is_pinned() {
    let bytes = generate(10_000, SEED);
    assert_eq!(
        format!("{:016x}", fnv1a(&bytes)),
        "c48e0a128a9b4f8a",
        "the generator's output changed (see this test's doc comment)"
    );
}

#[test]
fn file_name_carries_the_version() {
    assert_eq!(
        reference::file_name(),
        format!("reference-v{}.csv", reference::VERSION)
    );
}

#[test]
fn every_record_has_twelve_fields() {
    let bytes = generate(5_000, SEED);
    let records = records(&bytes);
    assert_eq!(records.len(), 5_001, "a header and one record per row");
    assert_eq!(HEADER.split(',').count(), COLUMNS);
    let header: Vec<&[u8]> = HEADER.as_bytes().split(|&b| b == b',').collect();
    assert_eq!(records[0], header);
    for (n, record) in records.iter().enumerate() {
        assert_eq!(
            record.len(),
            COLUMNS,
            "record {n} has {} fields",
            record.len()
        );
    }
}

#[test]
fn ids_count_up_from_one() {
    let bytes = generate(1_000, SEED);
    for (n, record) in records(&bytes).iter().skip(1).enumerate() {
        assert_eq!(record[0], (n + 1).to_string().as_bytes());
    }
}

/// DESIGN §1: UTF-8, with quoted fields that contain some newlines. The file
/// is otherwise regular (LF line endings, no CR, minimal quoting), so it
/// measures the common path, not the diagnostics.
#[test]
fn has_quoted_newlines_escaped_quotes_and_non_ascii() {
    let rows = 20_000;
    let bytes = generate(rows, SEED);
    let text = std::str::from_utf8(&bytes).expect("the file is UTF-8");
    assert!(!text.starts_with('\u{feff}'), "no BOM");
    assert!(!bytes.contains(&b'\r'), "LF line endings only");
    assert!(!text.is_ascii(), "some non-ASCII text");

    let records = records(&bytes);
    let multiline = records
        .iter()
        .filter(|record| record.iter().any(|field| field.contains(&b'\n')))
        .count();
    // "Some" newlines: between 1% and 5% of rows.
    assert!(
        (rows / 100..=rows / 20).contains(&(multiline as u64)),
        "{multiline} of {rows} rows have a newline in a field"
    );

    let escaped = records
        .iter()
        .flatten()
        .filter(|field| field.windows(2).any(|w| w == b"\"\""))
        .count();
    assert!(escaped > 0, "some fields contain escaped quotes");

    for field in records.iter().flatten() {
        let quoted = field.first() == Some(&b'"');
        let inner = if quoted {
            &field[1..field.len() - 1]
        } else {
            field
        };
        let needs_quotes = inner.iter().any(|b| matches!(b, b',' | b'"' | b'\n'));
        assert_eq!(
            quoted,
            needs_quotes,
            "fields are quoted exactly when they need it: {:?}",
            String::from_utf8_lossy(field)
        );
    }
}

/// DESIGN §1: about 100 MB for 1M rows. Estimated from the first 20,000
/// rows, allowing for the later rows' longer ids; `reference.sha256` pins
/// the exact size and contents (99,954,047 bytes).
#[test]
fn full_file_is_about_100_mb() {
    fn id_digits(rows: u64) -> u64 {
        (1..=rows).map(|id| id.to_string().len() as u64).sum()
    }
    let sample = 20_000;
    let bytes = generate(sample, SEED).len() as u64;
    let estimate = (bytes - id_digits(sample)) * (ROWS / sample) + id_digits(ROWS);
    assert!(
        (99_500_000..=100_500_000).contains(&estimate),
        "estimated size {estimate} bytes"
    );
}

#[test]
fn generate_writes_the_file_in_one_step() {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("reference-generate");
    let _ = std::fs::remove_dir_all(&dir);
    let path = dir.join("nested").join("small.csv");

    let written = reference::generate(&path, 300, SEED).unwrap();

    let contents = std::fs::read(&path).unwrap();
    assert_eq!(contents, generate(300, SEED));
    assert_eq!(written, contents.len() as u64);
    let leftovers: Vec<_> = std::fs::read_dir(path.parent().unwrap())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(leftovers, ["small.csv"], "no partial file is left behind");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn data_dir_is_under_the_workspace_target() {
    // `LEAL_BENCH_DATA` overrides it; the tests don't set it.
    if std::env::var_os("LEAL_BENCH_DATA").is_none() {
        let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let workspace = manifest.parent().unwrap().parent().unwrap();
        assert!(workspace.join("Cargo.lock").is_file());
        assert_eq!(
            reference::data_dir(),
            workspace.join("target").join("bench-data")
        );
    }
    assert_eq!(
        reference::path(),
        reference::data_dir().join(reference::file_name())
    );
}
