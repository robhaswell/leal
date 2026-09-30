//! Self-tests for the strategies: the model generator must round-trip
//! through the reference parser, `Messiness` must control what appears, and
//! both strategies must actually reach the interesting cases.

mod oracle;

use leal_testkit::diagnostics::{self, DiagnosticKind};
use leal_testkit::dialect::{Delimiter, Encoding, LineEnding, UTF8_BOM, expected_encoding};
use leal_testkit::fidelity::check_identical;
use leal_testkit::strategies::bytes::{INVALID_UTF8, MULTIBYTE_UTF8, SIGNIFICANT_BYTES, csv_bytes};
use leal_testkit::strategies::csv::{
    CsvConfig, GeneratedCsv, LineEndings, Messiness, ModelField, QuotingStyle, csv_file,
};
use proptest::prelude::*;
use proptest::sample::select;
use proptest::strategy::ValueTree;
use proptest::test_runner::TestRunner;

/// The generator's promise, checked against the independent oracle.
fn check_round_trip(file: &GeneratedCsv) -> Result<(), TestCaseError> {
    prop_assert_eq!(file.model.check(), Ok(()));
    prop_assert_eq!(file.encoding, expected_encoding(&file.bytes));
    prop_assert_eq!(
        file.layout.check_tiles(&file.bytes, file.delimiter()),
        Ok(())
    );

    let parsed = oracle::parse(&file.bytes, file.delimiter());
    prop_assert_eq!(
        &parsed,
        &file.layout,
        "oracle layout differs from the generator's"
    );
    prop_assert_eq!(
        oracle::to_model_rows(&parsed, &file.bytes),
        file.model.rows.clone(),
        "parsed model differs from the generated model"
    );

    // Rebuilding the file from the parsed spans gives the same bytes (F1 for
    // a trivial "serializer" that copies every span).
    let mut rebuilt = file.bytes[..parsed.bom_len].to_vec();
    for row in &parsed.rows {
        for (i, f) in row.fields.iter().enumerate() {
            if i > 0 {
                rebuilt.push(file.delimiter().byte());
            }
            rebuilt.extend_from_slice(&file.bytes[f.span.clone()]);
        }
        if let Some(le) = row.line_ending {
            rebuilt.extend_from_slice(le.bytes());
        }
    }
    check_identical(&file.bytes, &rebuilt)?;

    prop_assert_eq!(parsed.trailing_newline(), file.model.trailing_newline());
    if let LineEndings::Uniform(le) = file.model.dialect.line_endings {
        let (dominant, mixed) = parsed.line_endings();
        prop_assert!(!mixed);
        prop_assert!(dominant.is_none() || dominant == Some(le));
    }
    prop_assert_eq!(
        diagnostics::derive(&parsed, &file.bytes, file.encoding == Encoding::Utf8),
        file.diagnostics.clone()
    );
    Ok(())
}

fn kinds(file: &GeneratedCsv) -> Vec<DiagnosticKind> {
    file.diagnostics.iter().map(|d| d.kind).collect()
}

/// A config allowing exactly one irregular construct, by index 0..8, and the
/// diagnostic it may cause (`stray_quotes` causes none).
fn single_flag(i: usize) -> (CsvConfig, Option<DiagnosticKind>) {
    let mut m = Messiness::NONE;
    let kind = match i {
        0 => {
            m.mixed_line_endings = true;
            Some(DiagnosticKind::MixedLineEndings)
        }
        1 => {
            m.ragged_rows = true;
            Some(DiagnosticKind::RaggedRows)
        }
        2 => {
            m.blank_lines = true;
            Some(DiagnosticKind::BlankLines)
        }
        3 => {
            m.stray_quotes = true;
            None
        }
        4 => {
            m.text_after_closing_quote = true;
            Some(DiagnosticKind::TextAfterClosingQuote)
        }
        5 => {
            m.unterminated_quote = true;
            Some(DiagnosticKind::UnterminatedQuote)
        }
        6 => {
            m.invalid_utf8 = true;
            Some(DiagnosticKind::InvalidEncoding)
        }
        _ => {
            m.nul_bytes = true;
            Some(DiagnosticKind::NulBytes)
        }
    };
    (
        CsvConfig {
            messiness: m,
            ..CsvConfig::clean()
        },
        kind,
    )
}

proptest! {
    #[test]
    fn clean_files_round_trip(file in csv_file(CsvConfig::clean())) {
        check_round_trip(&file)?;
        // Clean files are valid UTF-8 (ASCII counts as UTF-8).
        prop_assert_eq!(file.encoding, Encoding::Utf8);
        // A clean file has at most the BOM info diagnostic.
        let k = kinds(&file);
        prop_assert!(k.is_empty() || k == vec![DiagnosticKind::BomPresent], "{:?}", k);
    }

    #[test]
    fn messy_files_round_trip(file in csv_file(CsvConfig::messy())) {
        check_round_trip(&file)?;
    }

    #[test]
    fn each_messiness_flag_controls_its_diagnostic(
        (i, file) in (0usize..8).prop_flat_map(|i| (Just(i), csv_file(single_flag(i).0)))
    ) {
        check_round_trip(&file)?;
        let allowed = single_flag(i).1;
        for k in kinds(&file) {
            prop_assert!(
                k == DiagnosticKind::BomPresent || Some(k) == allowed,
                "flag {} allowed {:?} but the file has {:?}", i, allowed, k
            );
        }
        if i == 3 {
            // Stray quotes are literal text in unquoted fields; no diagnostic.
            let has_trailing = |f: &ModelField| {
                matches!(f, ModelField::Quoted { trailing, .. } if !trailing.is_empty())
            };
            let any_trailing = file.model.rows.iter().flat_map(|r| &r.fields).any(has_trailing);
            prop_assert!(!any_trailing, "text after a closing quote without the flag");
        }
    }

    /// The oracle accepts any bytes, and its spans always tile the input.
    #[test]
    fn oracle_tiles_arbitrary_bytes(
        bytes in csv_bytes(),
        delimiter in select(&Delimiter::ALL[..]),
    ) {
        let layout = oracle::parse(&bytes, delimiter);
        prop_assert_eq!(layout.check_tiles(&bytes, delimiter), Ok(()));
        let _ = diagnostics::derive(&layout, &bytes, true);
    }
}

/// Draws `n` values from a strategy with a fixed seed, so coverage tests are
/// deterministic.
fn sample<S: Strategy>(strategy: S, n: usize) -> Vec<S::Value> {
    let mut runner = TestRunner::deterministic();
    (0..n)
        .map(|_| {
            strategy
                .new_tree(&mut runner)
                .expect("strategy failed")
                .current()
        })
        .collect()
}

#[test]
fn messy_generator_reaches_every_construct() {
    let files = sample(csv_file(CsvConfig::messy()), 2000);
    let has = |pred: &dyn Fn(&GeneratedCsv) -> bool| files.iter().any(pred);
    for kind in DiagnosticKind::ALL {
        assert!(
            has(&|f| kinds(f).contains(&kind)),
            "no generated file has {kind:?}"
        );
    }
    // Invalid bytes sometimes outnumber multibyte text (ADR-0003 decision 1).
    assert!(
        has(&|f| f.encoding == Encoding::Windows1252),
        "no Windows-1252 file"
    );
    let fields = |f: &GeneratedCsv| {
        f.layout
            .rows
            .iter()
            .flat_map(|r| r.fields.clone())
            .collect::<Vec<_>>()
    };
    assert!(
        has(&|f| fields(f)
            .iter()
            .any(|x| !x.quoted && x.value.contains(&b'"'))),
        "stray quote"
    );
    assert!(
        has(
            &|f| fields(f).iter().any(|x| x.text_after_quote.is_some() && {
                let t = &f.bytes[x.text_after_quote.unwrap()..x.span.end];
                t.contains(&b'"')
            })
        ),
        "quote inside text after a closing quote"
    );
    assert!(has(&|f| f.model.rows.iter().any(|r| r.fields.len() > 1)
        && f.layout.field_count_mode().is_some()));
}

#[test]
fn clean_generator_reaches_every_dialect() {
    let files = sample(csv_file(CsvConfig::clean()), 1000);
    let has = |pred: &dyn Fn(&GeneratedCsv) -> bool| files.iter().any(pred);
    for d in Delimiter::ALL {
        assert!(has(&|f| f.delimiter() == d), "{d:?}");
    }
    for le in LineEnding::ALL {
        assert!(has(&|f| f.layout.line_endings().0 == Some(le)), "{le:?}");
    }
    for q in [
        QuotingStyle::Minimal,
        QuotingStyle::Always,
        QuotingStyle::Mixed,
    ] {
        assert!(has(&|f| f.model.dialect.quoting == q), "{q:?}");
    }
    assert!(has(&|f| f.bytes.is_empty()), "empty file");
    assert!(has(&|f| f.model.dialect.bom), "BOM");
    assert!(
        has(&|f| !f.layout.rows.is_empty() && !f.layout.trailing_newline()),
        "no trailing newline"
    );
    assert!(has(&|f| f.layout.rows.len() == 1), "single row");
    assert!(
        has(&|f| f.layout.rows.len() > 3 && f.layout.field_counts().iter().all(|&n| n == 1)),
        "single column"
    );
    let quoted_values = |f: &GeneratedCsv| -> Vec<Vec<u8>> {
        f.layout
            .rows
            .iter()
            .flat_map(|r| &r.fields)
            .filter(|x| x.quoted)
            .map(|x| x.value.clone())
            .collect()
    };
    for needle in [&b"\n"[..], b"\r", b"\r\n", b"\"", b",", b";", b"\t", b"|"] {
        assert!(
            has(&|f| quoted_values(f)
                .iter()
                .any(|v| v.windows(needle.len()).any(|w| w == needle))),
            "no quoted value contains {:?}",
            needle.escape_ascii().to_string()
        );
    }
    assert!(
        has(&|f| quoted_values(f).iter().any(Vec::is_empty)),
        "quoted empty value"
    );
    assert!(
        has(&|f| String::from_utf8_lossy(&f.bytes).contains('😀')),
        "4-byte UTF-8"
    );
}

#[test]
fn byte_strategy_reaches_every_special_byte() {
    let inputs = sample(csv_bytes(), 500);
    let contains = |needle: &[u8]| {
        inputs
            .iter()
            .any(|b| b.windows(needle.len()).any(|w| w == needle))
    };
    for b in SIGNIFICANT_BYTES {
        assert!(contains(&[b]), "{b:#x}");
    }
    assert!(contains(b"\0"), "NUL");
    assert!(contains(b"\r\n"), "CRLF");
    assert!(contains(b"\",\""), "quote, delimiter, quote");
    assert!(
        inputs.iter().any(|b| b.starts_with(UTF8_BOM)),
        "leading BOM"
    );
    for seq in INVALID_UTF8 {
        assert!(contains(seq), "{seq:?}");
    }
    for s in MULTIBYTE_UTF8 {
        assert!(contains(s.as_bytes()), "{s}");
    }
    assert!(inputs.iter().any(Vec::is_empty), "empty input");
}
