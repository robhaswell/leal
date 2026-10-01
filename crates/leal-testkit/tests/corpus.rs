//! Corpus tests: every file loads with its sidecar, every sidecar agrees with
//! the reference parser, and the corpus covers every dialect and diagnostic.

mod oracle;

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use leal_testkit::corpus::{self, CorpusCase, SIDECAR_SUFFIX};
use leal_testkit::diagnostics::DiagnosticKind;
use leal_testkit::dialect::{Bom, Delimiter, Encoding, LineEnding, expected_encoding};

fn cases() -> Vec<CorpusCase> {
    corpus::load().unwrap_or_else(|e| panic!("{e}"))
}

#[test]
fn every_file_loads_with_its_sidecar() {
    let cases = cases();
    assert!(cases.len() >= 30, "only {} corpus cases", cases.len());
    for dir in ["dialect/", "diagnostics/", "exports/"] {
        assert!(
            cases.iter().any(|c| c.name.starts_with(dir)),
            "nothing in {dir}"
        );
    }
    // Every file under the corpus's subdirectories is either a data file
    // that loaded, or a sidecar of one.
    let mut all = Vec::new();
    walk(&corpus::corpus_dir(), &mut all);
    let data: BTreeSet<_> = cases.iter().map(|c| c.path.clone()).collect();
    for path in all {
        let is_top_level = path.parent() == Some(corpus::corpus_dir().as_path());
        let is_sidecar = path.to_string_lossy().ends_with(SIDECAR_SUFFIX);
        assert!(
            is_top_level || is_sidecar || data.contains(&path),
            "{} was not loaded",
            path.display()
        );
    }
}

#[test]
fn corpus_stays_small() {
    let mut all = Vec::new();
    walk(&corpus::corpus_dir(), &mut all);
    let total: u64 = all.iter().map(|p| fs::metadata(p).unwrap().len()).sum();
    assert!(total < 1_000_000, "corpus is {total} bytes");
}

#[test]
fn sidecars_match_the_reference_parser() {
    let mut problems = Vec::new();
    for case in cases() {
        check_case(&case, &mut problems);
    }
    assert!(
        problems.is_empty(),
        "{} mismatch(es):\n{}",
        problems.len(),
        problems.join("\n")
    );
}

fn check_case(case: &CorpusCase, problems: &mut Vec<String>) {
    let s = &case.sidecar;
    let mut fail = |what: String| problems.push(format!("{}: {what}", case.name));
    let a = oracle::analyze(&case.bytes, s.dialect.delimiter, s.dialect.encoding);
    let layout = &a.layout;

    if let Err(e) = a.check_tiles(s.dialect.delimiter) {
        fail(format!("layout does not tile: {e}"));
    }
    if layout.rows.len() != s.rows.count {
        fail(format!(
            "{} rows, sidecar says {}",
            layout.rows.len(),
            s.rows.count
        ));
    }
    if layout.field_counts() != s.rows.field_counts() {
        fail(format!(
            "field counts {:?}, sidecar says {:?}",
            layout.field_counts(),
            s.rows.field_counts()
        ));
    }
    if layout.trailing_newline() != s.dialect.trailing_newline {
        fail(format!("trailing newline is {}", layout.trailing_newline()));
    }
    let (dominant, mixed) = layout.line_endings();
    if dominant != s.dialect.line_ending || mixed != s.dialect.mixed_line_endings {
        fail(format!("line endings {dominant:?} mixed={mixed}"));
    }
    if Bom::detect(&case.bytes) != s.dialect.bom {
        fail(format!(
            "file starts with BOM {:?}",
            Bom::detect(&case.bytes)
        ));
    }

    for kind in DiagnosticKind::ALL {
        let got = a.diagnostics.iter().find(|d| d.kind == kind);
        match (got, s.diagnostic(kind)) {
            (None, None) => {}
            (Some(g), None) => fail(format!(
                "unexpected {kind:?} ({} at {:?})",
                g.count, g.first
            )),
            (None, Some(_)) => fail(format!("missing {kind:?}")),
            (Some(g), Some(e)) => {
                if g.count != e.count {
                    fail(format!(
                        "{kind:?} count {}, sidecar says {}",
                        g.count, e.count
                    ));
                }
                // Offsets are file offsets for every encoding (ADR-0003).
                if !g.first.starts_with(&e.first) {
                    fail(format!(
                        "{kind:?} at {:?}, sidecar says {:?}",
                        g.first, e.first
                    ));
                }
            }
        }
    }

    for cell in &s.cells {
        let at = format!("cell {}:{}", cell.row, cell.field);
        match a.display_value(cell.row, cell.field) {
            Some(v) if v == cell.value => {}
            Some(v) => fail(format!("{at} is {v:?}, sidecar says {:?}", cell.value)),
            None => fail(format!("{at} does not exist")),
        }
        if let Some(q) = cell.quoted
            && a.quoted(cell.row, cell.field) != Some(q)
        {
            fail(format!("{at} quoted is not {q}"));
        }
    }

    // The encoding follows ADR-0003 decision 1.
    let rule = expected_encoding(&case.bytes);
    if rule != s.dialect.encoding {
        fail(format!("ADR-0003 gives encoding {rule:?}"));
    }
    // A final odd byte in UTF-16 is not a whole code unit, so it is invalid
    // text at its own offset (tests/corpus/README.md). Corpus files keep the
    // rest of that field valid, so the field's first invalid byte is it.
    if matches!(s.dialect.encoding, Encoding::Utf16Le | Encoding::Utf16Be)
        && !case.bytes.len().is_multiple_of(2)
    {
        let last = case.bytes.len() - 1;
        let flagged = s
            .diagnostic(DiagnosticKind::InvalidEncoding)
            .is_some_and(|d| d.first.iter().any(|l| l.offset == last));
        if !flagged {
            fail(format!(
                "an odd-length UTF-16 file must expect invalid_encoding at offset {last}"
            ));
        }
    }
}

#[test]
fn corpus_covers_every_dialect_and_diagnostic() {
    let cases = cases();
    let has = |pred: &dyn Fn(&CorpusCase) -> bool| cases.iter().any(pred);
    for kind in DiagnosticKind::ALL {
        assert!(has(&|c| c.sidecar.diagnostic(kind).is_some()), "{kind:?}");
    }
    for d in Delimiter::ALL {
        assert!(has(&|c| c.sidecar.dialect.delimiter == d), "{d:?}");
    }
    for le in LineEnding::ALL {
        assert!(
            has(&|c| c.sidecar.dialect.line_ending == Some(le)),
            "{le:?}"
        );
    }
    for bom in [Bom::None, Bom::Utf8, Bom::Utf16Le, Bom::Utf16Be] {
        assert!(has(&|c| c.sidecar.dialect.bom == bom), "{bom:?}");
    }
    for enc in [
        Encoding::Utf8,
        Encoding::Utf16Le,
        Encoding::Utf16Be,
        Encoding::Windows1252,
    ] {
        assert!(has(&|c| c.sidecar.dialect.encoding == enc), "{enc:?}");
    }
    assert!(
        has(&|c| matches!(
            c.sidecar.dialect.encoding,
            Encoding::Utf16Le | Encoding::Utf16Be
        ) && !c.bytes.len().is_multiple_of(2)),
        "a UTF-16 file with an odd number of bytes"
    );
    assert!(has(&|c| c.sidecar.dialect.mixed_line_endings));
    assert!(has(&|c| c.sidecar.dialect.header));
    assert!(has(
        &|c| !c.sidecar.dialect.header && c.sidecar.rows.count > 1
    ));
    assert!(has(
        &|c| !c.sidecar.dialect.trailing_newline && c.sidecar.rows.count > 0
    ));
    assert!(has(&|c| c.bytes.is_empty()));
    assert!(has(
        &|c| c.sidecar.rows.count > 1 && c.sidecar.rows.field_counts().iter().all(|&n| n == 1)
    ));
    assert!(has(&|c| c.sidecar.rows.count == 1));
    for name in [
        "excel-utf8-bom",
        "excel-windows-1252",
        "google-sheets",
        "numbers",
        "pandas",
        "postgresql",
    ] {
        assert!(
            has(&|c| c.name.starts_with(&format!("exports/imitation-{name}"))),
            "{name}"
        );
    }
}

// ---- loader error handling -----------------------------------------------------

/// A scratch corpus directory, removed when dropped.
struct TempCorpus(PathBuf);

impl TempCorpus {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("leal-testkit-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("sub")).unwrap();
        TempCorpus(dir)
    }

    fn write(&self, rel: &str, contents: &[u8]) {
        fs::write(self.0.join(rel), contents).unwrap();
    }
}

impl Drop for TempCorpus {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

const MINIMAL_SIDECAR: &str = r#"
description = "x"
[dialect]
delimiter = ","
line_ending = "lf"
mixed_line_endings = false
bom = "none"
encoding = "utf-8"
trailing_newline = true
header = false
[rows]
count = 1
fields = [1]
"#;

#[test]
fn loader_reports_every_problem() {
    let t = TempCorpus::new("problems");
    t.write("README.md", b"top-level files are ignored");
    t.write(".DS_Store", b"hidden files are ignored");
    t.write("sub/.hidden", b"hidden files are ignored");
    t.write("sub/good.csv", b"a\n");
    t.write("sub/good.csv.expected.toml", MINIMAL_SIDECAR.as_bytes());
    t.write("sub/lonely.csv", b"a\n");
    t.write("sub/orphan.csv.expected.toml", MINIMAL_SIDECAR.as_bytes());
    t.write("sub/bad.csv", b"a\n");
    t.write("sub/bad.csv.expected.toml", b"description = 1");

    let err = corpus::load_from(&t.0).unwrap_err();
    let text = err.to_string();
    assert_eq!(err.problems.len(), 3, "{text}");
    assert!(text.contains("sub/lonely.csv: no sidecar"), "{text}");
    assert!(
        text.contains("sub/orphan.csv.expected.toml: sidecar has no data file"),
        "{text}"
    );
    assert!(text.contains("sub/bad.csv.expected.toml: "), "{text}");
}

#[test]
fn loader_loads_a_good_corpus() {
    let t = TempCorpus::new("good");
    t.write("sub/good.csv", b"a\n");
    t.write("sub/good.csv.expected.toml", MINIMAL_SIDECAR.as_bytes());
    let cases = corpus::load_from(&t.0).unwrap();
    assert_eq!(cases.len(), 1);
    assert_eq!(cases[0].name, "sub/good.csv");
    assert_eq!(cases[0].bytes, b"a\n");
    assert_eq!(cases[0].sidecar.rows.count, 1);
}

#[test]
fn loader_rejects_an_empty_or_missing_corpus() {
    let t = TempCorpus::new("empty");
    assert!(
        corpus::load_from(&t.0)
            .unwrap_err()
            .to_string()
            .contains("no corpus files")
    );
    let missing = t.0.join("nope");
    assert!(
        corpus::load_from(&missing)
            .unwrap_err()
            .to_string()
            .contains("cannot read")
    );
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).unwrap().flatten() {
        let path = entry.path();
        if path
            .file_name()
            .is_some_and(|n| n.to_string_lossy().starts_with('.'))
        {
            continue;
        }
        if path.is_dir() {
            walk(&path, out);
        } else {
            out.push(path);
        }
    }
}
