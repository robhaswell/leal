//! Detection against every corpus sidecar (`tests/corpus/`, task 0.2):
//! delimiter, line endings, BOM, encoding, trailing newline and the
//! hand-written header expectations.

mod common;

use leal_core::detect::{Choices, DialectSource, EncodingSource, Hints, detect, review};
use leal_core::dialect::Bom;

#[test]
fn detection_matches_every_corpus_sidecar() {
    let cases = leal_testkit::corpus::load().expect("the corpus loads");
    let mut problems = Vec::new();
    for case in &cases {
        let expected = &case.sidecar.dialect;
        let d = detect(&case.bytes, Hints::default(), Choices::default())
            .expect("no choices, so no choice error");
        let mut check = |what: &str, got: String, want: String| {
            if got != want {
                problems.push(format!("{}: {what} is {got}, expected {want}", case.name));
            }
        };
        check(
            "delimiter",
            format!("{:?}", d.delimiter),
            format!("{:?}", common::delimiter(expected.delimiter)),
        );
        check(
            "line ending",
            format!("{:?}", d.line_ending),
            format!("{:?}", expected.line_ending.map(common::line_ending)),
        );
        check(
            "mixed line endings",
            d.mixed_line_endings.to_string(),
            expected.mixed_line_endings.to_string(),
        );
        check(
            "BOM",
            format!("{:?}", d.bom),
            format!("{:?}", common::bom(expected.bom)),
        );
        check(
            "encoding",
            format!("{:?}", d.encoding),
            format!("{:?}", common::encoding(expected.encoding)),
        );
        // Every corpus file is under 64 KB, so first paint sees all of it.
        check(
            "trailing newline",
            format!("{:?}", d.trailing_newline),
            format!("{:?}", Some(expected.trailing_newline)),
        );
        check("header", d.header.to_string(), expected.header.to_string());

        let source = if d.bom == Bom::None {
            EncodingSource::Guess
        } else {
            EncodingSource::Bom
        };
        check(
            "encoding source",
            format!("{:?}", d.encoding_source),
            format!("{source:?}"),
        );
        check(
            "delimiter and header sources",
            format!("{:?}", (d.delimiter_source, d.header_source)),
            format!("{:?}", (DialectSource::Guess, DialectSource::Guess)),
        );
        check("notes", format!("{:?}", d.notes), "[]".to_owned());

        // The whole-file review agrees with first paint on a small file.
        let r = review(&case.bytes, &d);
        check(
            "review",
            format!(
                "{:?}",
                (
                    r.encoding_suggestion,
                    r.delimiter_suggestion,
                    r.line_ending,
                    r.mixed_line_endings,
                    r.trailing_newline
                )
            ),
            format!(
                "{:?}",
                (
                    None::<()>,
                    None::<()>,
                    d.line_ending,
                    d.mixed_line_endings,
                    expected.trailing_newline
                )
            ),
        );
    }
    assert!(
        problems.is_empty(),
        "{} problem(s) in {} corpus files:\n{}",
        problems.len(),
        cases.len(),
        problems.join("\n")
    );
}

/// The sidecars' header expectations cover both answers, including files
/// with more than one row and no header (PLAN 1.2, 0.2 obligation).
#[test]
fn the_corpus_tests_headers_both_ways() {
    let cases = leal_testkit::corpus::load().expect("the corpus loads");
    assert!(cases.iter().any(|c| c.sidecar.dialect.header));
    assert!(
        cases
            .iter()
            .any(|c| !c.sidecar.dialect.header && c.sidecar.rows.count > 1)
    );
}
