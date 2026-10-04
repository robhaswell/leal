//! Shared code for Leal's fuzz targets (PLAN 2.7, DESIGN §5 test layer 4).
//!
//! Every target checks leal-core against the testkit, never against
//! itself:
//!
//! - [`oracle`] is the testkit's naive reference parser
//!   (`crates/leal-testkit/tests/oracle/mod.rs`), compiled in here by path.
//!   It lives in the testkit's integration tests so that product code can't
//!   use it; this crate is test code, like those tests.
//! - [`harness`] opens a real document, replays an edit script on it and
//!   on the save oracle (`leal_testkit::save::Document`), saves to memory
//!   and reopens what it wrote.
//!
//! A failed check panics, which libFuzzer reports as a crash and saves the
//! input under `fuzz/artifacts/<target>/`.

#[path = "../../crates/leal-testkit/tests/oracle/mod.rs"]
pub mod oracle;

pub mod harness;

use leal_core::diagnostics::{DiagnosticKind, Report};
use leal_core::dialect::{Bom, Delimiter, Encoding, LineEnding, QUOTE};
use leal_core::index::IndexDialect;
use leal_testkit::diagnostics::{
    Diagnostic as TkDiagnostic, DiagnosticKind as TkKind, Location as TkLocation,
};
use leal_testkit::dialect as tk;

/// The testkit's name for one of Leal's encodings (the two lists are the
/// same 16).
pub fn tk_encoding(encoding: Encoding) -> tk::Encoding {
    [
        tk::Encoding::Utf8,
        tk::Encoding::Utf16Le,
        tk::Encoding::Utf16Be,
    ]
    .into_iter()
    .chain(tk::Encoding::SINGLE_BYTE)
    .find(|theirs| theirs.name() == encoding.iana_name())
    .expect("the testkit has every encoding Leal has")
}

/// Leal's name for one of the testkit's encodings.
pub fn core_encoding(encoding: tk::Encoding) -> Encoding {
    Encoding::ALL
        .into_iter()
        .find(|ours| ours.iana_name() == encoding.name())
        .expect("Leal has every encoding the testkit has")
}

/// The testkit's delimiter for one of Leal's.
pub fn tk_delimiter(delimiter: Delimiter) -> tk::Delimiter {
    tk::Delimiter::from_byte(delimiter.byte()).expect("the same four delimiters")
}

/// The testkit's line ending for one of Leal's.
pub fn tk_line_ending(line_ending: LineEnding) -> tk::LineEnding {
    match line_ending {
        LineEnding::Lf => tk::LineEnding::Lf,
        LineEnding::Crlf => tk::LineEnding::Crlf,
        LineEnding::Cr => tk::LineEnding::Cr,
    }
}

/// The index dialect for `encoding` and `delimiter`, with a BOM of
/// `bom_len` bytes.
pub fn index_dialect(delimiter: Delimiter, encoding: Encoding, bom_len: usize) -> IndexDialect {
    IndexDialect {
        delimiter: delimiter.byte(),
        quote: QUOTE,
        code_unit: encoding.code_unit(),
        bom_len,
    }
}

/// The ways the raw-bytes targets read `bytes`: the encodings it can be
/// read in, given its BOM. A UTF-16 BOM allows only that UTF-16, and a
/// UTF-8 BOM only UTF-8. Without a BOM: UTF-8, Windows-1252 (detection's
/// two), and one more single-byte encoding, picked by length, so that
/// display values in every table get fuzzed too.
pub fn readings(bytes: &[u8]) -> Vec<(Encoding, usize)> {
    let bom = Bom::detect(bytes);
    match bom.encoding() {
        Some(encoding) => vec![(encoding, bom.len())],
        None => {
            let others = &tk::Encoding::SINGLE_BYTE[1..];
            let other = core_encoding(others[bytes.len() % others.len()]);
            vec![(Encoding::Utf8, 0), (Encoding::Windows1252, 0), (other, 0)]
        }
    }
}

/// A diagnostics report in the testkit's terms.
pub fn tk_diagnostics(report: &Report) -> Vec<TkDiagnostic> {
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
