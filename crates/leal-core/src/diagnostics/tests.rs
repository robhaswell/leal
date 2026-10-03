//! Tests for diagnostics: every corpus sidecar, the testkit's generated
//! files (clean and messy, UTF-8, Windows-1252 and UTF-16), arbitrary bytes,
//! files with thousands of occurrences, reports made while indexing, and
//! hand-written cases.
//!
//! The expected answers come from the testkit: the sidecars, the
//! generator's diagnostics, and `leal_testkit::diagnostics::derive`, the
//! testkit's definition of what counts as one occurrence and where it is.
//! For arbitrary bytes, `derive` is given the layout the row parser (1.4)
//! produces, which 1.4 checks against the generator and the corpus. The
//! testkit's reference parser is never used here (0.2 notes).

use super::*;

use std::sync::atomic::{AtomicBool, Ordering};

use leal_testkit::corpus::{self, CorpusCase};
use leal_testkit::diagnostics::{
    self as tk, Diagnostic as TkDiagnostic, DiagnosticKind as TkKind, Location as TkLocation,
    Severity as TkSeverity,
};
use leal_testkit::dialect::{Encoding as TkEncoding, LineEnding as TkLineEnding};
use leal_testkit::layout::{FieldLayout, Layout, RowLayout};
use leal_testkit::strategies::bytes::csv_bytes;
use leal_testkit::strategies::csv::{CsvConfig, GeneratedCsv, csv_file, csv_file_utf16};
use proptest::prelude::*;

use crate::index::{CHUNK_BYTES, CodeUnit, LineEnding, RowIndex, Status};
use crate::rows::RowParser;

// ---------------------------------------------------------------------------
// Helpers

fn dialect(delimiter: u8, encoding: Encoding, bom_len: usize) -> IndexDialect {
    IndexDialect {
        delimiter,
        quote: b'"',
        code_unit: encoding.code_unit(),
        bom_len,
    }
}

fn utf8() -> IndexDialect {
    dialect(b',', Encoding::Utf8, 0)
}

fn from_tk(encoding: TkEncoding) -> Encoding {
    Encoding::ALL
        .into_iter()
        .find(|e| e.iana_name() == encoding.name())
        .unwrap()
}

fn tk_kind(kind: DiagnosticKind) -> TkKind {
    match kind {
        DiagnosticKind::UnterminatedQuote => TkKind::UnterminatedQuote,
        DiagnosticKind::RaggedRows => TkKind::RaggedRows,
        DiagnosticKind::TextAfterClosingQuote => TkKind::TextAfterClosingQuote,
        DiagnosticKind::InvalidEncoding => TkKind::InvalidEncoding,
        DiagnosticKind::NulBytes => TkKind::NulBytes,
        DiagnosticKind::MixedLineEndings => TkKind::MixedLineEndings,
        DiagnosticKind::BlankLines => TkKind::BlankLines,
        DiagnosticKind::BomPresent => TkKind::BomPresent,
    }
}

fn tk_severity(severity: Severity) -> TkSeverity {
    match severity {
        Severity::Info => TkSeverity::Info,
        Severity::Warning => TkSeverity::Warning,
        Severity::Error => TkSeverity::Error,
    }
}

/// A report in the testkit's terms, to compare with its answers.
fn to_tk(report: &Report) -> Vec<TkDiagnostic> {
    report
        .diagnostics()
        .iter()
        .map(|d| TkDiagnostic {
            kind: tk_kind(d.kind()),
            count: d.count(),
            first: d
                .first()
                .iter()
                .map(|l| TkLocation {
                    row: l.row,
                    offset: l.offset,
                })
                .collect(),
        })
        .collect()
}

/// Indexes `bytes` through the progressive path, `chunk` bytes at a time,
/// and returns the index, the diagnostics and the final report.
fn collect_all(
    bytes: &[u8],
    dialect: IndexDialect,
    encoding: Encoding,
    chunk: usize,
) -> (Arc<RowIndex>, Arc<Diagnostics>, Report) {
    let (index, diagnostics, indexer) =
        RowIndex::start_with_diagnostics(dialect, encoding).unwrap();
    indexer
        .with_chunk_bytes(chunk)
        .run(bytes, &AtomicBool::new(false), |_| {})
        .unwrap();
    let report = Arc::unwrap_or_clone(diagnostics.report());
    assert!(report.is_complete());
    assert_eq!(report.rows(), index.row_count());
    (index, diagnostics, report)
}

/// [`collect_all`]'s index and report.
fn collect(
    bytes: &[u8],
    dialect: IndexDialect,
    encoding: Encoding,
    chunk: usize,
) -> (Arc<RowIndex>, Report) {
    let (index, _, report) = collect_all(bytes, dialect, encoding, chunk);
    (index, report)
}

/// [`collect_all`]'s report in the testkit's terms, after checking every
/// row's mark against [`expected_marks`].
fn collect_tk(
    bytes: &[u8],
    dialect: IndexDialect,
    encoding: TkEncoding,
    chunk: usize,
) -> Result<Vec<TkDiagnostic>, TestCaseError> {
    let (index, diagnostics, report) = collect_all(bytes, dialect, from_tk(encoding), chunk);
    let want = expected_marks(bytes, dialect, encoding);
    prop_assert_eq!(want.len(), index.row_count());
    check_marks(&diagnostics, &want)?;
    Ok(to_tk(&report))
}

/// Which rows should be marked (a warning or an error), worked out
/// independently of the collector: from the row parser's layout (ragged
/// rows, text after a closing quote, the unterminated quote) and from the
/// testkit's untruncated lists of NULs and invalid text.
fn expected_marks(bytes: &[u8], dialect: IndexDialect, encoding: TkEncoding) -> Vec<bool> {
    let layout = layout_of(bytes, dialect, from_tk(encoding));
    let mode = layout.field_count_mode();
    let mut marks: Vec<bool> = layout
        .rows
        .iter()
        .map(|r| {
            (!r.is_blank() && Some(r.fields.len()) != mode)
                || r.fields
                    .iter()
                    .any(|f| f.unterminated || f.text_after_quote.is_some())
        })
        .collect();
    let nul_bytes = || {
        bytes
            .iter()
            .enumerate()
            .filter(|(_, b)| **b == 0)
            .map(|(i, _)| i)
    };
    let offsets: Vec<usize> = match encoding {
        TkEncoding::Utf8 => nul_bytes().chain(tk::invalid_utf8_offsets(bytes)).collect(),
        TkEncoding::Utf16Le | TkEncoding::Utf16Be => {
            let (nuls, invalid) = tk::utf16_nul_and_invalid_offsets(
                bytes,
                dialect.bom_len,
                encoding == TkEncoding::Utf16Le,
            );
            nuls.into_iter().chain(invalid).collect()
        }
        single_byte => {
            let unassigned = leal_testkit::dialect::unassigned_bytes(single_byte);
            nul_bytes()
                .chain(
                    bytes
                        .iter()
                        .enumerate()
                        .filter(|&(_, &b)| unassigned[usize::from(b)])
                        .map(|(i, _)| i),
                )
                .collect()
        }
    };
    for offset in offsets {
        // A NUL or invalid byte is always inside a field (and so a row),
        // since the structural characters are ASCII.
        let row = layout.row_of_offset(offset).expect("in a row");
        marks[row] = true;
    }
    marks
}

/// Checks [`Diagnostics::row_has_diagnostic`] against `want` for every
/// row (and one past the end), and the next and previous marked rows from
/// a spread of rows.
fn check_marks(diagnostics: &Diagnostics, want: &[bool]) -> Result<(), TestCaseError> {
    for (row, &marked) in want.iter().enumerate() {
        prop_assert_eq!(diagnostics.row_has_diagnostic(row), marked, "row {}", row);
    }
    prop_assert!(!diagnostics.row_has_diagnostic(want.len()));
    let rows = want.len();
    let step = (rows / 50).max(1);
    for at in (0..=rows + 1).step_by(step).chain([rows, rows + 1]) {
        let next = (at.min(rows)..rows).find(|&r| want[r]);
        let previous = (0..at.min(rows)).rev().find(|&r| want[r]);
        prop_assert_eq!(
            diagnostics.next_row_with_diagnostic(at),
            next,
            "next from {}",
            at
        );
        prop_assert_eq!(
            diagnostics.previous_row_with_diagnostic(at),
            previous,
            "previous before {}",
            at
        );
    }
    Ok(())
}

fn to_tk_line_ending(le: LineEnding) -> TkLineEnding {
    match le {
        LineEnding::Lf => TkLineEnding::Lf,
        LineEnding::Crlf => TkLineEnding::Crlf,
        LineEnding::Cr => TkLineEnding::Cr,
    }
}

/// The row parser's layout of `bytes`. Values are left empty: `derive`
/// doesn't use them.
fn layout_of(bytes: &[u8], dialect: IndexDialect, encoding: Encoding) -> Layout {
    let index = RowIndex::build(bytes, dialect).unwrap();
    let parser = RowParser::new(dialect, encoding).unwrap();
    Layout {
        bom_len: dialect.bom_len,
        rows: (0..index.row_count())
            .map(|r| {
                let row = index.row(r, bytes).unwrap();
                let parsed = parser.parse(bytes, row.span.clone()).unwrap();
                RowLayout {
                    span: row.span,
                    line_ending: row.line_ending.map(to_tk_line_ending),
                    fields: parsed
                        .fields()
                        .iter()
                        .map(|f| FieldLayout {
                            span: f.span(),
                            quoted: f.quoted(),
                            value: Vec::new(),
                            text_after_quote: f.text_after_quote(),
                            unterminated: f.unterminated(),
                        })
                        .collect(),
                }
            })
            .collect(),
    }
}

/// What the testkit says the diagnostics of `bytes` are, for the layout the
/// row parser finds.
fn expected(bytes: &[u8], dialect: IndexDialect, encoding: TkEncoding) -> Vec<TkDiagnostic> {
    tk::derive(
        &layout_of(bytes, dialect, from_tk(encoding)),
        bytes,
        encoding,
    )
}

/// Checks a generated file's diagnostics against the generator's, in
/// chunks of `chunk` bytes.
fn check_generated(file: &GeneratedCsv, chunk: usize) -> Result<(), TestCaseError> {
    let encoding = from_tk(file.encoding);
    let d = dialect(file.delimiter().byte(), encoding, file.layout.bom_len);
    prop_assert_eq!(
        collect_tk(&file.bytes, d, file.encoding, chunk)?,
        file.diagnostics.clone()
    );
    Ok(())
}

/// The file's encoding and dialect for arbitrary bytes: `choice` picks
/// UTF-8, Windows-1252, UTF-16 LE or UTF-16 BE. UTF-16 gets a BOM in front;
/// UTF-8 has one if the bytes start with one.
fn arbitrary(bytes: &[u8], delimiter: u8, choice: usize) -> (Vec<u8>, IndexDialect, TkEncoding) {
    let encoding = [
        TkEncoding::Utf8,
        TkEncoding::Windows1252,
        TkEncoding::Utf16Le,
        TkEncoding::Utf16Be,
    ][choice];
    let (bytes, bom_len) = match encoding {
        TkEncoding::Utf8 => (
            bytes.to_vec(),
            if bytes.starts_with(b"\xEF\xBB\xBF") {
                3
            } else {
                0
            },
        ),
        TkEncoding::Utf16Le => ([&[0xFF, 0xFE], bytes].concat(), 2),
        TkEncoding::Utf16Be => ([&[0xFE, 0xFF], bytes].concat(), 2),
        _ => (bytes.to_vec(), 0),
    };
    (
        bytes,
        dialect(delimiter, from_tk(encoding), bom_len),
        encoding,
    )
}

fn utf16(text: &str, little_endian: bool) -> Vec<u8> {
    let mut bytes = if little_endian {
        vec![0xFF, 0xFE]
    } else {
        vec![0xFE, 0xFF]
    };
    for unit in text.encode_utf16() {
        bytes.extend_from_slice(&if little_endian {
            unit.to_le_bytes()
        } else {
            unit.to_be_bytes()
        });
    }
    bytes
}

fn utf16_dialect(little_endian: bool) -> (IndexDialect, Encoding) {
    let encoding = if little_endian {
        Encoding::Utf16Le
    } else {
        Encoding::Utf16Be
    };
    (dialect(b',', encoding, 2), encoding)
}

/// The report of comma-delimited UTF-8 bytes.
fn report_of(bytes: &[u8]) -> Report {
    RowIndex::build_with_diagnostics(bytes, utf8(), Encoding::Utf8)
        .unwrap()
        .1
}

/// `(count, first locations as (row, offset))` of one kind.
fn found(report: &Report, kind: DiagnosticKind) -> Option<(usize, Vec<(usize, usize)>)> {
    report.get(kind).map(|d| {
        (
            d.count(),
            d.first().iter().map(|l| (l.row, l.offset)).collect(),
        )
    })
}

/// Rows of `fields` fields each, `a,a,…` with LF line endings.
fn rows_of_fields(counts: impl IntoIterator<Item = usize>) -> Vec<u8> {
    let mut bytes = Vec::new();
    for fields in counts {
        bytes.extend_from_slice(vec!["a"; fields].join(",").as_bytes());
        bytes.push(b'\n');
    }
    bytes
}

// ---------------------------------------------------------------------------
// The corpus

#[test]
fn every_corpus_sidecar_matches() {
    let cases = corpus::load().unwrap();
    assert!(cases.len() >= 42, "only {} corpus files", cases.len());
    let mut kinds_seen = std::collections::BTreeSet::new();
    for case in &cases {
        let CorpusCase {
            name,
            bytes,
            sidecar,
            ..
        } = case;
        let encoding = from_tk(sidecar.dialect.encoding);
        let d = dialect(
            sidecar.dialect.delimiter.byte(),
            encoding,
            sidecar.dialect.bom.bytes().len(),
        );
        let (_, built) = RowIndex::build_with_diagnostics(bytes, d, encoding).unwrap();
        for chunk in [CHUNK_BYTES, 1, 2, 3, 5, 16] {
            let (_, report) = collect(bytes, d, encoding, chunk);
            assert_eq!(report, built, "{name}: chunks of {chunk}");
        }

        // Exactly the kinds the sidecar lists ("Kinds not listed must NOT
        // be reported").
        let mut want: Vec<TkKind> = sidecar.diagnostics.iter().map(|d| d.kind).collect();
        want.sort();
        let got: Vec<TkKind> = built
            .diagnostics()
            .iter()
            .map(|d| tk_kind(d.kind()))
            .collect();
        assert_eq!(got, want, "{name}: kinds");
        for expected in &sidecar.diagnostics {
            let got = built
                .diagnostics()
                .iter()
                .find(|d| tk_kind(d.kind()) == expected.kind)
                .unwrap();
            assert_eq!(
                got.count(),
                expected.count,
                "{name}: {:?} count",
                expected.kind
            );
            let first: Vec<TkLocation> = got
                .first()
                .iter()
                .map(|l| TkLocation {
                    row: l.row,
                    offset: l.offset,
                })
                .collect();
            assert!(
                first.starts_with(&expected.first),
                "{name}: {:?} at {first:?}, expected {:?}",
                expected.kind,
                expected.first
            );
            assert_eq!(first.len(), got.count().min(MAX_LOCATIONS), "{name}");
            assert_eq!(
                tk_severity(got.severity()),
                expected.kind.severity(),
                "{name}: {:?} severity",
                expected.kind
            );
            kinds_seen.insert(expected.kind);
        }
    }
    assert_eq!(
        kinds_seen.len(),
        DiagnosticKind::ALL.len(),
        "every kind is in the corpus"
    );
}

// ---------------------------------------------------------------------------
// Generated files and arbitrary bytes

proptest! {
    #[test]
    fn clean_files_match_the_generator(file in csv_file(CsvConfig::clean()), chunk in 1..48usize) {
        check_generated(&file, chunk)?;
    }

    #[test]
    fn messy_files_match_the_generator(file in csv_file(CsvConfig::messy()), chunk in 1..48usize) {
        check_generated(&file, chunk)?;
        check_generated(&file, CHUNK_BYTES)?;
    }

    #[test]
    fn messy_utf16_files_match_the_generator(
        file in csv_file_utf16(CsvConfig::messy()),
        chunk in 1..48usize,
    ) {
        check_generated(&file, chunk)?;
        check_generated(&file, CHUNK_BYTES)?;
    }

    #[test]
    fn messy_files_with_long_values_match_the_generator(
        file in csv_file(CsvConfig { max_rows: 4, max_fields: 3, max_value_chunks: 100, ..CsvConfig::messy() }),
        chunk in 1..200usize,
    ) {
        check_generated(&file, chunk)?;
    }

    /// Any bytes, in UTF-8, Windows-1252 and UTF-16, against the testkit's
    /// definition applied to the row parser's fields. This reaches what the
    /// generator doesn't: unpaired surrogates, NULs and invalid sequences
    /// next to delimiters and quotes, BOMs in the middle, and so on.
    #[test]
    fn any_bytes_match_the_testkit(
        bytes in csv_bytes(),
        delimiter in prop::sample::select(&b",;\t|"[..]),
        choice in 0..4usize,
        chunk in 1..64usize,
    ) {
        let (bytes, d, encoding) = arbitrary(&bytes, delimiter, choice);
        let want = expected(&bytes, d, encoding);
        prop_assert_eq!(&collect_tk(&bytes, d, encoding, chunk)?, &want);
        prop_assert_eq!(&collect_tk(&bytes, d, encoding, CHUNK_BYTES)?, &want);
    }
}

/// Rows built for counts above [`MAX_LOCATIONS`]: mostly two or three
/// fields, so the most common count can change late, with blank lines,
/// every line ending, NULs, invalid bytes and text after a closing quote.
fn many_rows() -> impl Strategy<Value = Vec<u8>> {
    let field = prop_oneof![
        20 => Just(&b"a"[..]),
        1 => Just(&b"\0"[..]),
        1 => Just(&b"\xFF"[..]),
        1 => Just(&b"\"q\"x"[..]),
        1 => Just(&b""[..]),
    ];
    let fields = prop_oneof![
        8 => Just(2usize),
        8 => Just(3usize),
        1 => 1..6usize,
    ];
    let row = (
        prop::bool::weighted(0.3),
        fields.prop_flat_map(move |n| prop::collection::vec(field.clone(), n)),
        prop_oneof![6 => Just(&b"\n"[..]), 2 => Just(&b"\r\n"[..]), 1 => Just(&b"\r"[..])],
    );
    prop::collection::vec(row, 0..5000).prop_map(|rows| {
        let mut bytes = Vec::new();
        for (blank, fields, ending) in rows {
            if !blank {
                bytes.extend_from_slice(&fields.join(&b","[..]));
            }
            bytes.extend_from_slice(ending);
        }
        bytes
    })
}

/// A property over [`many_rows`]. Each case takes about 0.1 s in a debug
/// build, so it runs 48 cases, or a hundredth of `PROPTEST_CASES` if that
/// is more (200 in `just test-deep`), rather than `PROPTEST_CASES` itself,
/// which would take over half an hour.
#[test]
fn thousands_of_occurrences_match_the_testkit() {
    use proptest::test_runner::{Config, TestRunner};
    let deep = std::env::var("PROPTEST_CASES")
        .ok()
        .and_then(|n| n.parse::<u32>().ok())
        .map_or(0, |n| n / 100);
    let mut runner = TestRunner::new(Config {
        cases: deep.max(48),
        ..Config::default()
    });
    runner
        .run(&(many_rows(), 1..4096usize), |(bytes, chunk)| {
            let want = expected(&bytes, utf8(), TkEncoding::Utf8);
            prop_assert_eq!(&collect_tk(&bytes, utf8(), TkEncoding::Utf8, chunk)?, &want);
            Ok(())
        })
        .unwrap();
}

#[test]
fn many_rows_reach_the_location_limit() {
    // The strategy's files are big enough to need truncation: check that a
    // sample of them has more than MAX_LOCATIONS of the common kinds.
    use proptest::strategy::ValueTree;
    use proptest::test_runner::TestRunner;
    let mut runner = TestRunner::deterministic();
    let mut over = std::collections::BTreeSet::new();
    for _ in 0..20 {
        let bytes = many_rows().new_tree(&mut runner).unwrap().current();
        for d in report_of(&bytes).diagnostics() {
            if d.count() > MAX_LOCATIONS {
                over.insert(d.kind());
            }
        }
    }
    for kind in [
        DiagnosticKind::RaggedRows,
        DiagnosticKind::MixedLineEndings,
        DiagnosticKind::BlankLines,
    ] {
        assert!(over.contains(&kind), "{kind:?} never went over the limit");
    }
}

// ---------------------------------------------------------------------------
// While indexing

/// Checks that every report published while indexing describes exactly the
/// rows indexed so far: it equals the testkit's diagnostics of the file cut
/// after the last of those rows, and so do the rows' marks.
fn check_provisional(
    bytes: &[u8],
    dialect: IndexDialect,
    encoding: TkEncoding,
    chunk: usize,
) -> Result<(), TestCaseError> {
    let (index, diagnostics, indexer) =
        RowIndex::start_with_diagnostics(dialect, from_tk(encoding)).unwrap();
    let mut failure = None;
    let mut reports = 0;
    indexer
        .with_chunk_bytes(chunk)
        .run(bytes, &AtomicBool::new(false), |progress| {
            reports += 1;
            if failure.is_none() {
                failure = describes_rows_so_far(bytes, dialect, encoding, &index, &diagnostics)
                    .and_then(|()| {
                        let rows = diagnostics.report().rows();
                        (rows == progress.rows)
                            .then_some(())
                            .ok_or_else(|| format!("{rows} rows, progress says {}", progress.rows))
                    })
                    .err();
            }
        })
        .unwrap();
    prop_assert!(reports > 0);
    if let Some(failure) = failure {
        prop_assert!(false, "{}", failure);
    }
    Ok(())
}

/// Whether the latest report and marks describe exactly the rows indexed
/// so far, as [`check_provisional`] requires.
fn describes_rows_so_far(
    bytes: &[u8],
    dialect: IndexDialect,
    encoding: TkEncoding,
    index: &RowIndex,
    diagnostics: &Diagnostics,
) -> Result<(), String> {
    let report = diagnostics.report();
    let rows = report.rows();
    if rows != index.row_count() {
        return Err(format!("{rows} rows, the index has {}", index.row_count()));
    }
    let end = if rows == 0 {
        dialect.bom_len
    } else {
        index.row_extent(rows - 1).unwrap().end
    };
    let want = expected(&bytes[..end], dialect, encoding);
    let done = index.status() == Status::Complete;
    if report.is_complete() != done || to_tk(&report) != want {
        return Err(format!(
            "after {rows} rows (done: {done}): {:?}, expected {want:?}",
            to_tk(&report)
        ));
    }
    check_marks(
        diagnostics,
        &expected_marks(&bytes[..end], dialect, encoding),
    )
    .map_err(|e| format!("after {rows} rows, marks: {e}"))
}

/// Checks the chunked path (a removable drive's [`Source::stream`],
/// 1.3a): `bytes` given to a [`ChunkedIndexer`] in chunks of `chunk`
/// bytes. After every chunk the report and marks describe exactly the rows
/// so far, and at the end they equal the testkit's for the whole file.
///
/// [`Source::stream`]: crate::source::Source::stream
/// [`ChunkedIndexer`]: crate::index::ChunkedIndexer
fn check_chunked(
    bytes: &[u8],
    dialect: IndexDialect,
    encoding: TkEncoding,
    chunk: usize,
) -> Result<(), TestCaseError> {
    let (index, diagnostics, indexer) =
        RowIndex::start_with_diagnostics(dialect, from_tk(encoding)).unwrap();
    let mut chunked = indexer.chunked(bytes.len()).unwrap();
    for piece in bytes.chunks(chunk.max(1)) {
        chunked.push(piece).unwrap();
        describes_rows_so_far(bytes, dialect, encoding, &index, &diagnostics)
            .map_err(TestCaseError::fail)?;
    }
    chunked.finish().unwrap();
    prop_assert_eq!(index.status(), Status::Complete);
    describes_rows_so_far(bytes, dialect, encoding, &index, &diagnostics)
        .map_err(TestCaseError::fail)?;
    prop_assert_eq!(
        to_tk(&diagnostics.report()),
        expected(bytes, dialect, encoding)
    );
    Ok(())
}

proptest! {
    #[test]
    fn reports_while_indexing_describe_the_rows_so_far(
        file in csv_file(CsvConfig::messy()),
        chunk in 1..32usize,
    ) {
        let d = dialect(file.delimiter().byte(), from_tk(file.encoding), file.layout.bom_len);
        check_provisional(&file.bytes, d, file.encoding, chunk)?;
    }

    #[test]
    fn utf16_reports_while_indexing_describe_the_rows_so_far(
        file in csv_file_utf16(CsvConfig::messy()),
        chunk in 1..32usize,
    ) {
        let d = dialect(file.delimiter().byte(), from_tk(file.encoding), file.layout.bom_len);
        check_provisional(&file.bytes, d, file.encoding, chunk)?;
    }

    #[test]
    fn reports_while_indexing_any_bytes_describe_the_rows_so_far(
        bytes in csv_bytes(),
        choice in 0..4usize,
        chunk in 1..32usize,
    ) {
        let (bytes, d, encoding) = arbitrary(&bytes, b',', choice);
        check_provisional(&bytes, d, encoding, chunk)?;
    }

    /// The chunked path, cut anywhere (inside UTF-8 sequences, surrogate
    /// pairs, CRLFs and `""`), gives the same diagnostics as a whole-file
    /// index, and describes the rows so far after every chunk.
    #[test]
    fn chunked_files_match_the_testkit(
        file in csv_file(CsvConfig::messy()),
        chunk in 1..40usize,
    ) {
        let d = dialect(file.delimiter().byte(), from_tk(file.encoding), file.layout.bom_len);
        check_chunked(&file.bytes, d, file.encoding, chunk)?;
    }

    #[test]
    fn chunked_utf16_files_match_the_testkit(
        file in csv_file_utf16(CsvConfig::messy()),
        chunk in 1..40usize,
    ) {
        let d = dialect(file.delimiter().byte(), from_tk(file.encoding), file.layout.bom_len);
        check_chunked(&file.bytes, d, file.encoding, chunk)?;
    }

    #[test]
    fn chunked_any_bytes_match_the_testkit(
        bytes in csv_bytes(),
        delimiter in prop::sample::select(&b",;\t|"[..]),
        choice in 0..4usize,
        chunk in 1..40usize,
    ) {
        let (bytes, d, encoding) = arbitrary(&bytes, delimiter, choice);
        check_chunked(&bytes, d, encoding, chunk)?;
    }
}

/// Every cut of a short file full of sequences that straddle chunk
/// boundaries: valid and invalid UTF-8, a NUL, text after a quote, CRLF and
/// a quoted CRLF, cut once and twice.
#[test]
fn chunked_diagnostics_are_the_same_wherever_the_chunks_are_cut() {
    let bytes = "é,\"a\r\nb\"x\r\n\u{1F600}\0,\u{20AC}\n".as_bytes();
    let mut bytes = bytes.to_vec();
    bytes.extend_from_slice(b"\xE2\x82,\xF0\x9F\x98\n\"q\"\xFF\r\nend\xC3");
    let want = expected(&bytes, utf8(), TkEncoding::Utf8);
    assert!(want.len() >= 4, "{want:?}");
    for i in 0..=bytes.len() {
        for j in i..=bytes.len() {
            let (index, diagnostics, indexer) =
                RowIndex::start_with_diagnostics(utf8(), Encoding::Utf8).unwrap();
            let mut chunked = indexer.chunked(bytes.len()).unwrap();
            for piece in [&bytes[..i], &bytes[i..j], &bytes[j..]] {
                chunked.push(piece).unwrap();
            }
            chunked.finish().unwrap();
            assert_eq!(index.status(), Status::Complete);
            assert_eq!(to_tk(&diagnostics.report()), want, "cut at {i} and {j}");
            let marks = expected_marks(&bytes, utf8(), TkEncoding::Utf8);
            check_marks(&diagnostics, &marks).unwrap();
        }
    }
}

/// The most common field count changes late: 1,500 rows of two fields,
/// then 1,600 of three. While only the first rows are indexed, the
/// three-field rows are ragged; at the end, the two-field rows are, and the
/// first 1,000 of them must still be known.
#[test]
fn ragged_rows_follow_the_mode_as_it_changes() {
    let bytes = rows_of_fields(std::iter::repeat_n(2, 1500).chain(std::iter::repeat_n(3, 1600)));
    let report = report_of(&bytes);
    let (count, first) = found(&report, DiagnosticKind::RaggedRows).unwrap();
    assert_eq!(count, 1500);
    let rows: Vec<usize> = first.iter().map(|&(row, _)| row).collect();
    assert_eq!(rows, (0..MAX_LOCATIONS).collect::<Vec<_>>());
    assert_eq!(first[1], (1, 4));
    check_provisional(&bytes, utf8(), TkEncoding::Utf8, 997).unwrap();

    // And a mode that changes only after more than 2,000 rows.
    let bytes = rows_of_fields(
        std::iter::repeat_n(2, 1200)
            .chain(std::iter::repeat_n(3, 1100))
            .chain(std::iter::repeat_n(3, 300))
            .chain(std::iter::repeat_n(4, 50)),
    );
    let report = report_of(&bytes);
    let (count, first) = found(&report, DiagnosticKind::RaggedRows).unwrap();
    assert_eq!(count, 1250);
    assert_eq!(first.len(), MAX_LOCATIONS);
    assert_eq!(first[0].0, 0);
    assert_eq!(first[MAX_LOCATIONS - 1].0, MAX_LOCATIONS - 1);
    check_provisional(&bytes, utf8(), TkEncoding::Utf8, 4096).unwrap();

    // And ragged rows that start only after 2,000 rows, once the mode has
    // a clear majority: the first 1,000 of them are kept, then no more.
    let bytes = rows_of_fields(std::iter::repeat_n(3, 2500).chain(std::iter::repeat_n(2, 1200)));
    let report = report_of(&bytes);
    let (count, first) = found(&report, DiagnosticKind::RaggedRows).unwrap();
    assert_eq!(count, 1200);
    let rows: Vec<usize> = first.iter().map(|&(row, _)| row).collect();
    assert_eq!(rows, (2500..2500 + MAX_LOCATIONS).collect::<Vec<_>>());
    check_provisional(&bytes, utf8(), TkEncoding::Utf8, 8192).unwrap();
}

/// A cancelled run leaves its last report incomplete, describing the rows
/// indexed before it stopped.
#[test]
fn a_cancelled_run_leaves_an_incomplete_report() {
    let bytes = rows_of_fields((0..300).map(|i| 2 + i % 3));
    let (index, diagnostics, indexer) =
        RowIndex::start_with_diagnostics(utf8(), Encoding::Utf8).unwrap();
    let cancel = AtomicBool::new(false);
    let mut calls = 0;
    let result = indexer.with_chunk_bytes(64).run(&bytes, &cancel, |_| {
        calls += 1;
        if calls == 3 {
            cancel.store(true, Ordering::Relaxed);
        }
    });
    assert_eq!(result, Err(IndexError::Cancelled));
    assert_eq!(index.status(), Status::Stopped);
    let report = diagnostics.report();
    assert!(!report.is_complete());
    assert_eq!(report.rows(), index.row_count());
    let end = index.row_extent(report.rows() - 1).unwrap().end;
    assert_eq!(
        to_tk(&report),
        expected(&bytes[..end], utf8(), TkEncoding::Utf8)
    );
    assert!(report.get(DiagnosticKind::RaggedRows).is_some());
}

#[test]
fn a_report_is_empty_and_incomplete_until_the_indexer_runs() {
    let (_, diagnostics, indexer) =
        RowIndex::start_with_diagnostics(utf8(), Encoding::Utf8).unwrap();
    assert_eq!(*diagnostics.report(), Report::default());
    drop(indexer);
    let report = diagnostics.report();
    assert!(!report.is_complete());
    assert_eq!(report.rows(), 0);
    assert_eq!(diagnostics.encoding(), Encoding::Utf8);
}

#[test]
fn the_plain_index_paths_still_work_without_diagnostics() {
    let bytes = b"a,b\n\"x\"y\n";
    let (index, report) = RowIndex::build_with_diagnostics(bytes, utf8(), Encoding::Utf8).unwrap();
    let plain = RowIndex::build(bytes, utf8()).unwrap();
    assert_eq!(index.row_count(), plain.row_count());
    assert_eq!(index.field_count_mode(), plain.field_count_mode());
    assert_eq!(report.rows(), 2);
}

// ---------------------------------------------------------------------------
// Hand-written cases

#[test]
fn an_empty_file_has_no_diagnostics() {
    let report = report_of(b"");
    assert!(report.diagnostics().is_empty());
    assert!(report.is_complete());
    assert!(!report.shows_banner());
}

#[test]
fn a_bom_is_info_only() {
    let d = dialect(b',', Encoding::Utf8, 3);
    for bytes in [&b"\xEF\xBB\xBF"[..], b"\xEF\xBB\xBFa,b\n"] {
        let (_, report) = RowIndex::build_with_diagnostics(bytes, d, Encoding::Utf8).unwrap();
        assert_eq!(
            found(&report, DiagnosticKind::BomPresent),
            Some((1, vec![(0, 0)]))
        );
        assert_eq!(report.diagnostics().len(), 1);
        assert!(!report.shows_banner());
        assert_eq!(report.kinds_at_least(Severity::Info), 1);
        assert_eq!(report.kinds_at_least(Severity::Warning), 0);
    }
}

#[test]
fn an_unterminated_quote_is_one_error_at_its_opening_quote() {
    // The NULs after the quoted newline are in the same, last field.
    let bytes = b"a,b\nc,\"d\n\0\0\n,\0";
    let report = report_of(bytes);
    assert_eq!(
        found(&report, DiagnosticKind::UnterminatedQuote),
        Some((1, vec![(1, 6)]))
    );
    assert_eq!(
        found(&report, DiagnosticKind::NulBytes),
        Some((1, vec![(1, 9)]))
    );
    assert_eq!(
        report
            .get(DiagnosticKind::UnterminatedQuote)
            .unwrap()
            .severity(),
        Severity::Error
    );
    assert_eq!(report.kinds_at_least(Severity::Error), 1);
    assert_eq!(report.kinds_at_least(Severity::Warning), 2);
}

#[test]
fn field_kinds_count_once_per_field() {
    // Row 0: two NULs in field 0, one in field 2. Row 1: invalid bytes in a
    // quoted field across a newline, then text after its closing quote.
    let bytes = b"\0x\0,y,\0\n\"\xFF\n\xFE\"z\0,\xC3\n";
    let report = report_of(bytes);
    assert_eq!(
        found(&report, DiagnosticKind::NulBytes),
        Some((3, vec![(0, 0), (0, 6), (1, 14)]))
    );
    assert_eq!(
        found(&report, DiagnosticKind::InvalidEncoding),
        Some((2, vec![(1, 9), (1, 16)]))
    );
    assert_eq!(
        found(&report, DiagnosticKind::TextAfterClosingQuote),
        Some((1, vec![(1, 13)]))
    );
    assert_eq!(to_tk(&report), expected(bytes, utf8(), TkEncoding::Utf8));
}

#[test]
fn locations_stop_at_the_limit_but_the_count_does_not() {
    let bytes = "\0,".repeat(1500).into_bytes();
    let report = report_of(&bytes);
    let nul = report.get(DiagnosticKind::NulBytes).unwrap();
    assert_eq!(nul.count(), 1500);
    assert_eq!(nul.first().len(), MAX_LOCATIONS);
    assert_eq!(
        nul.first()[999],
        Location {
            row: 0,
            offset: 1998
        }
    );
}

#[test]
fn utf8_split_across_chunks_is_valid_and_cut_off_at_the_end_is_not() {
    let bytes = "é€😀,x\n".as_bytes();
    for chunk in 1..=bytes.len() {
        assert!(
            collect(bytes, utf8(), Encoding::Utf8, chunk)
                .1
                .diagnostics()
                .is_empty(),
            "{chunk}"
        );
    }
    let bytes = b"a,\xF0\x9F\x98";
    for chunk in 1..=bytes.len() {
        let (_, report) = collect(bytes, utf8(), Encoding::Utf8, chunk);
        assert_eq!(
            found(&report, DiagnosticKind::InvalidEncoding),
            Some((1, vec![(0, 2)]))
        );
    }
}

#[test]
fn utf16_nuls_are_code_units_and_lone_surrogates_are_invalid() {
    for little_endian in [true, false] {
        let (d, encoding) = utf16_dialect(little_endian);
        // "a" holds a 0x00 byte but isn't a NUL; U+0000 is. A pair (😀)
        // split across chunks is valid; a lone low surrogate isn't.
        let mut bytes = utf16("a,\u{0}b,😀\n", little_endian);
        let lone_low = if little_endian {
            [0x00, 0xDC]
        } else {
            [0xDC, 0x00]
        };
        bytes.extend_from_slice(&lone_low);
        for chunk in [1, 2, 3, 4, 5, 7, CHUNK_BYTES] {
            let (_, report) = collect(&bytes, d, encoding, chunk);
            assert_eq!(
                found(&report, DiagnosticKind::NulBytes),
                Some((1, vec![(0, 6)])),
                "{chunk}"
            );
            assert_eq!(
                found(&report, DiagnosticKind::InvalidEncoding),
                Some((1, vec![(1, 18)])),
                "{chunk}"
            );
        }
    }
}

#[test]
fn utf16_text_after_a_closing_quote_can_be_a_final_odd_byte() {
    let (d, encoding) = utf16_dialect(true);
    let mut bytes = utf16("\"a\"", true);
    bytes.push(b'x');
    let (_, report) = RowIndex::build_with_diagnostics(&bytes, d, encoding).unwrap();
    assert_eq!(
        found(&report, DiagnosticKind::TextAfterClosingQuote),
        Some((1, vec![(0, 8)]))
    );
    assert_eq!(
        found(&report, DiagnosticKind::InvalidEncoding),
        Some((1, vec![(0, 8)]))
    );
    assert_eq!(to_tk(&report), expected(&bytes, d, TkEncoding::Utf16Le));
}

/// Every byte a single-byte encoding doesn't map displays as U+FFFD (1.4),
/// so it is invalid text; every byte it maps isn't. Windows-1252,
/// ISO-8859-1 and Mac Roman map every byte.
#[test]
fn unmapped_single_byte_bytes_are_invalid_text() {
    let mut any_unmapped = false;
    for encoding in Encoding::ALL {
        if encoding.code_unit() != CodeUnit::Byte || encoding == Encoding::Utf8 {
            continue;
        }
        let d = dialect(b',', encoding, 0);
        let parser = RowParser::new(d, encoding).unwrap();
        for b in 0x80..=0xFF_u8 {
            let bytes = [b'x', b, b'y', b',', b, b'\n'];
            let (_, report) = RowIndex::build_with_diagnostics(&bytes, d, encoding).unwrap();
            let row = parser.parse(&bytes, 0..5).unwrap();
            let shown = parser.display_value(&bytes, &row.fields()[0]);
            let unmapped = shown.contains(char::REPLACEMENT_CHARACTER);
            let want = unmapped.then(|| (2, vec![(0, 1), (0, 4)]));
            assert_eq!(
                found(&report, DiagnosticKind::InvalidEncoding),
                want,
                "{encoding:?} byte {b:#04X}"
            );
            any_unmapped |= unmapped;
            if matches!(
                encoding,
                Encoding::Windows1252 | Encoding::Iso8859_1 | Encoding::MacRoman
            ) {
                assert!(!unmapped, "{encoding:?} byte {b:#04X}");
            }
        }
    }
    assert!(
        any_unmapped,
        "some encoding leaves a byte unmapped (Windows-1253 0xAA)"
    );
}

/// The coordinator's decision on the 1.5 open question: in Windows-1253,
/// which leaves 0xAA, 0xD2 and 0xFF unmapped (they display as U+FFFD), each
/// field with one is `invalid_encoding` once, at its first such byte.
#[test]
fn windows_1253_unmapped_bytes_are_invalid_once_per_field() {
    // Row 0: Greek text (all mapped), then a field with two unmapped bytes.
    // Row 1: an unmapped byte in a quoted field, and one alone. Row 2: none.
    let bytes = b"\xC1\xE8\xDE\xED\xE1,x\xAAy\xD2\n\"\xFF\",\xAA\n\xE1,\xE2\n";
    let d = dialect(b',', Encoding::Windows1253, 0);
    let (_, diagnostics, report) = collect_all(bytes, d, Encoding::Windows1253, CHUNK_BYTES);
    assert_eq!(
        found(&report, DiagnosticKind::InvalidEncoding),
        Some((3, vec![(0, 7), (1, 12), (1, 15)]))
    );
    assert_eq!(report.diagnostics().len(), 1);
    let marks: Vec<bool> = (0..3).map(|r| diagnostics.row_has_diagnostic(r)).collect();
    assert_eq!(marks, [true, true, false]);
    // The same bytes in Windows-1252, which maps all of them, are clean.
    let d = dialect(b',', Encoding::Windows1252, 0);
    let (_, report) = RowIndex::build_with_diagnostics(bytes, d, Encoding::Windows1252).unwrap();
    assert!(report.diagnostics().is_empty());
}

/// Rows of 127 fields or more keep their exact count on the side, so a
/// wide file's ragged rows are marked correctly, and so are wide rows in a
/// narrow file.
#[test]
fn wide_rows_are_marked_by_their_exact_field_count() {
    let counts = [200, 200, 201, 200, 127, 5, 200, 126, 128];
    let bytes = rows_of_fields(counts);
    let (_, diagnostics, report) = collect_all(&bytes, utf8(), Encoding::Utf8, 97);
    assert_eq!(report.get(DiagnosticKind::RaggedRows).unwrap().count(), 5);
    let marks: Vec<bool> = (0..counts.len())
        .map(|r| diagnostics.row_has_diagnostic(r))
        .collect();
    let want: Vec<bool> = counts.iter().map(|&n| n != 200).collect();
    assert_eq!(marks, want);
    assert_eq!(diagnostics.next_row_with_diagnostic(3), Some(4));
    assert_eq!(diagnostics.previous_row_with_diagnostic(4), Some(2));

    // A narrow file: every wide row is ragged.
    let counts = [3, 3, 130, 3, 300, 3];
    let bytes = rows_of_fields(counts);
    let (_, diagnostics, _) = collect_all(&bytes, utf8(), Encoding::Utf8, CHUNK_BYTES);
    let marks: Vec<bool> = (0..counts.len())
        .map(|r| diagnostics.row_has_diagnostic(r))
        .collect();
    assert_eq!(marks, [false, false, true, false, true, false]);
    assert_eq!(diagnostics.next_row_with_diagnostic(3), Some(4));
    assert_eq!(diagnostics.next_row_with_diagnostic(5), None);
    assert_eq!(diagnostics.previous_row_with_diagnostic(2), None);
}

/// Wide files: rows of 127 fields or more, most of them with the most
/// common count, so `next` and `previous` walk the side list of wide rows.
fn wide_rows() -> impl Strategy<Value = Vec<u8>> {
    let row = (
        prop_oneof![
            8 => Just(130usize),
            1 => Just(129usize),
            1 => Just(131usize),
            1 => Just(127usize),
            1 => Just(126usize),
            1 => Just(3usize),
            1 => Just(0usize),
        ],
        prop::bool::weighted(0.05),
    );
    prop::collection::vec(row, 0..120).prop_map(|rows| {
        let mut bytes = Vec::new();
        for (fields, nul) in rows {
            if fields > 0 {
                let mut cells = vec![&b"a"[..]; fields];
                if nul {
                    cells[fields / 2] = b"\0";
                }
                bytes.extend_from_slice(&cells.join(&b","[..]));
            }
            bytes.push(b'\n');
        }
        bytes
    })
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 64, ..ProptestConfig::default() })]

    #[test]
    fn wide_files_are_marked_and_searched_correctly(bytes in wide_rows(), chunk in 1..2000usize) {
        prop_assert_eq!(
            &collect_tk(&bytes, utf8(), TkEncoding::Utf8, chunk)?,
            &expected(&bytes, utf8(), TkEncoding::Utf8)
        );
        // From every row, not just a spread of them.
        let (_, diagnostics, _) = collect_all(&bytes, utf8(), Encoding::Utf8, chunk);
        let want = expected_marks(&bytes, utf8(), TkEncoding::Utf8);
        for at in 0..=want.len() + 1 {
            let next = (at.min(want.len())..want.len()).find(|&r| want[r]);
            let previous = (0..at.min(want.len())).rev().find(|&r| want[r]);
            prop_assert_eq!(diagnostics.next_row_with_diagnostic(at), next, "next from {}", at);
            prop_assert_eq!(diagnostics.previous_row_with_diagnostic(at), previous, "before {}", at);
        }
        // Each kind's walk (task 1.7's Previous and Next) agrees with its
        // row-by-row test: in a wide mode, each `Mark` has its own walk.
        for which in [Mark::Any, Mark::Ragged, Mark::Flagged] {
            let rows: Vec<bool> = (0..want.len()).map(|r| diagnostics.row_is(r, which)).collect();
            for at in 0..=rows.len() + 1 {
                let next = (at.min(rows.len())..rows.len()).find(|&r| rows[r]);
                let previous = (0..at.min(rows.len())).rev().find(|&r| rows[r]);
                prop_assert_eq!(
                    diagnostics.rows_where(at, which, true, 1).first().copied(),
                    next,
                    "{:?} next from {}", which, at
                );
                prop_assert_eq!(
                    diagnostics.rows_where(at, which, false, 1).first().copied(),
                    previous,
                    "{:?} before {}", which, at
                );
            }
        }
    }
}

/// Once indexing is complete, the row marks give back the room they grew
/// into, as the row index's offsets do.
#[test]
fn row_marks_are_shrunk_when_indexing_finishes() {
    // 100,000 narrow rows and 10 wide ones (8 bytes each on the side).
    let mut counts: Vec<usize> = vec![2; 100_000];
    for i in 0..10 {
        counts[i * 9000] = 150;
    }
    let bytes = rows_of_fields(counts);
    let (_, diagnostics, report) = collect_all(&bytes, utf8(), Encoding::Utf8, 64 * 1024);
    assert!(report.is_complete());
    assert_eq!(diagnostics.marks_capacity(), 100_000 + 10 * 8);
}

/// Every affected row is marked, not just the first 1,000 of each kind,
/// and info-level kinds (blank lines, mixed line endings, the BOM) mark
/// nothing. Long runs of unmarked rows are skipped 64 rows at a time.
#[test]
fn every_row_with_a_warning_is_marked() {
    let mut bytes = b"\xEF\xBB\xBFa,b\r\n".to_vec();
    for i in 0..5000 {
        bytes.extend_from_slice(match i % 500 {
            0 => b"\0,b\n".as_slice(),
            1 => b"a\n",
            2 => b"\n",
            _ => b"a,b\n",
        });
    }
    let d = dialect(b',', Encoding::Utf8, 3);
    let (_, diagnostics, report) = collect_all(&bytes, d, Encoding::Utf8, 4096);
    assert_eq!(report.get(DiagnosticKind::NulBytes).unwrap().count(), 10);
    assert_eq!(report.get(DiagnosticKind::RaggedRows).unwrap().count(), 10);
    let marked: Vec<usize> = (0..5001)
        .filter(|&r| diagnostics.row_has_diagnostic(r))
        .collect();
    let want: Vec<usize> = (0..5000).filter(|i| i % 500 < 2).map(|i| i + 1).collect();
    assert_eq!(marked, want);
    assert_eq!(diagnostics.next_row_with_diagnostic(3), Some(501));
    assert_eq!(diagnostics.previous_row_with_diagnostic(501), Some(2));
    assert_eq!(diagnostics.previous_row_with_diagnostic(1), None);
    assert_eq!(diagnostics.next_row_with_diagnostic(4503), None);
    let lots = "\0\n".repeat(2500);
    let (_, diagnostics, report) = collect_all(lots.as_bytes(), utf8(), Encoding::Utf8, 1000);
    assert_eq!(
        report.get(DiagnosticKind::NulBytes).unwrap().first().len(),
        MAX_LOCATIONS
    );
    assert!(diagnostics.row_has_diagnostic(2499));
}

#[test]
fn nuls_count_in_single_byte_encodings_too() {
    let d = dialect(b';', Encoding::Windows1250, 0);
    let (_, report) =
        RowIndex::build_with_diagnostics(b"a;\0\n", d, Encoding::Windows1250).unwrap();
    assert_eq!(
        found(&report, DiagnosticKind::NulBytes),
        Some((1, vec![(0, 2)]))
    );
}

/// With a NUL delimiter or quote, 0x00 is structure, not text.
#[test]
fn a_nul_delimiter_or_quote_is_not_a_nul_in_a_field() {
    let d = dialect(0, Encoding::Utf8, 0);
    let (_, report) = RowIndex::build_with_diagnostics(b"a\0b\n", d, Encoding::Utf8).unwrap();
    assert!(report.diagnostics().is_empty());
    let d = IndexDialect { quote: 0, ..utf8() };
    let (_, report) = RowIndex::build_with_diagnostics(b"\0a\0,b\n", d, Encoding::Utf8).unwrap();
    assert!(report.diagnostics().is_empty());
}

/// A file of UTF-16 text without a BOM, read as Windows-1252: every other
/// byte is a NUL. Each field counts once, however many it holds.
#[test]
fn utf16_without_a_bom_read_as_bytes_has_a_nul_in_every_field() {
    let mut bytes = Vec::new();
    for unit in "ab,cd,e\n".repeat(200).encode_utf16() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    let d = dialect(b',', Encoding::Windows1252, 0);
    let (_, report) = RowIndex::build_with_diagnostics(&bytes, d, Encoding::Windows1252).unwrap();
    let nul = report.get(DiagnosticKind::NulBytes).unwrap();
    // 200 rows of 3 fields (each LF's 0x00 starts the next row's first
    // field), then the last LF's 0x00 on its own.
    assert_eq!(nul.count(), 601);
    assert_eq!(to_tk(&report), expected(&bytes, d, TkEncoding::Windows1252));
}

#[test]
fn mixed_line_endings_ties_go_to_the_first_seen() {
    // One CRLF, one LF, one CR: CRLF is first, so the LF and CR rows mix.
    let report = report_of(b"a\r\nb\nc\rd");
    assert_eq!(
        found(&report, DiagnosticKind::MixedLineEndings),
        Some((2, vec![(1, 4), (2, 6)]))
    );
    // The last row has no line ending, so it doesn't count.
    assert_eq!(
        found(&report, DiagnosticKind::MixedLineEndings).unwrap().0,
        2
    );
}

#[test]
fn blank_lines_are_never_ragged() {
    let report = report_of(b"a,b\n\n\nc,d\n\n");
    assert_eq!(
        found(&report, DiagnosticKind::BlankLines),
        Some((3, vec![(1, 4), (2, 5), (4, 10)]))
    );
    assert!(report.get(DiagnosticKind::RaggedRows).is_none());
}

#[test]
fn a_dialect_and_encoding_must_agree() {
    let want = IndexError::EncodingMismatch {
        code_unit: CodeUnit::Byte,
        encoding: Encoding::Utf16Le,
    };
    assert_eq!(
        RowIndex::build_with_diagnostics(b"a", utf8(), Encoding::Utf16Le).unwrap_err(),
        want
    );
    assert_eq!(
        RowIndex::start_with_diagnostics(utf8(), Encoding::Utf16Le).unwrap_err(),
        want
    );
    let (d, _) = utf16_dialect(true);
    assert!(matches!(
        RowIndex::start_with_diagnostics(d, Encoding::Utf8),
        Err(IndexError::EncodingMismatch { .. })
    ));
    assert!(want.to_string().contains("Utf16Le"));
}

#[test]
fn kinds_and_severities_match_the_design_table() {
    let ours: Vec<TkKind> = DiagnosticKind::ALL.into_iter().map(tk_kind).collect();
    assert_eq!(ours, TkKind::ALL.to_vec());
    for kind in DiagnosticKind::ALL {
        assert_eq!(
            tk_severity(kind.severity()),
            tk_kind(kind).severity(),
            "{kind:?}"
        );
    }
    let mut sorted = DiagnosticKind::ALL;
    sorted.sort();
    assert_eq!(sorted, DiagnosticKind::ALL);
}

#[test]
fn diagnostics_are_shared_between_threads() {
    fn shareable<T: Send + Sync>() {}
    shareable::<Diagnostics>();
    shareable::<Report>();
}

#[test]
fn rows_with_the_common_field_count_leave_out_blank_and_ragged_rows() {
    // Six rows: a blank line, a short row and a long one; three have the
    // common count of two fields.
    let bytes = b"a,b\n1,2\n\n3\n4,5,6\n7,8\n";
    let (_, report) =
        RowIndex::build_with_diagnostics(bytes, dialect(b',', Encoding::Utf8, 0), Encoding::Utf8)
            .unwrap();
    assert_eq!(report.rows(), 6);
    assert_eq!(report.rows_with_common_field_count(), 3);
    assert_eq!(Report::default().rows_with_common_field_count(), 0);
}
