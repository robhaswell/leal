//! The hand-made test corpus in `tests/corpus/` and its sidecar files.
//!
//! Every data file lives in a subdirectory of `tests/corpus/` and has a
//! sidecar next to it named `<file>.expected.toml` (for example
//! `dialect/comma.csv.expected.toml`). Files directly in `tests/corpus/`
//! (the README and the generator script) and hidden files are not data.
//! The sidecar format and coordinate conventions are documented in
//! `tests/corpus/README.md`.
//!
//! ```
//! for case in leal_testkit::corpus::load().unwrap() {
//!     assert_eq!(case.sidecar.rows.count, case.sidecar.rows.field_counts().len());
//! }
//! ```

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Deserializer};

use crate::diagnostics::{Diagnostic, DiagnosticKind, MAX_LOCATIONS};
use crate::dialect::{Bom, Delimiter, Encoding, LineEnding};

/// The suffix that marks a sidecar file.
pub const SIDECAR_SUFFIX: &str = ".expected.toml";

/// The absolute path of `tests/corpus/` in this repository.
#[must_use]
pub fn corpus_dir() -> PathBuf {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/corpus");
    // Resolve the `..`s for readable messages; fall back if it doesn't exist.
    dir.canonicalize().unwrap_or(dir)
}

/// The sidecar path for a data file: its name plus [`SIDECAR_SUFFIX`].
#[must_use]
pub fn sidecar_path(data: &Path) -> PathBuf {
    let mut name = data.as_os_str().to_owned();
    name.push(SIDECAR_SUFFIX);
    PathBuf::from(name)
}

/// One corpus file with its parsed sidecar.
#[derive(Clone, Debug)]
pub struct CorpusCase {
    /// The path relative to the corpus directory, with `/` separators, for
    /// example `dialect/comma.csv`.
    pub name: String,
    /// The absolute path of the data file.
    pub path: PathBuf,
    /// The data file's bytes.
    pub bytes: Vec<u8>,
    /// What a parser should find in it.
    pub sidecar: Sidecar,
}

/// Why the corpus could not be loaded. Lists every problem found, not just
/// the first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CorpusError {
    /// One line per problem.
    pub problems: Vec<String>,
}

impl fmt::Display for CorpusError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "the test corpus has {} problem(s):", self.problems.len())?;
        for p in &self.problems {
            writeln!(f, "  - {p}")?;
        }
        Ok(())
    }
}

impl std::error::Error for CorpusError {}

/// Loads every case in [`corpus_dir`], sorted by name.
///
/// # Errors
///
/// See [`load_from`].
pub fn load() -> Result<Vec<CorpusCase>, CorpusError> {
    load_from(&corpus_dir())
}

/// Loads every case under `dir`, sorted by name.
///
/// # Errors
///
/// Returns every problem found: unreadable files, data files without a
/// sidecar, sidecars without a data file, and sidecars that don't parse or
/// fail [`Sidecar::validate`]. An empty corpus is also an error.
pub fn load_from(dir: &Path) -> Result<Vec<CorpusCase>, CorpusError> {
    let mut problems = Vec::new();
    let mut files = Vec::new();
    match fs::read_dir(dir) {
        Ok(entries) => {
            for entry in entries.flatten() {
                let path = entry.path();
                // Top-level files (README, generator) are not data.
                if path.is_dir() && !is_hidden(&path) {
                    collect(&path, &mut files, &mut problems);
                }
            }
        }
        Err(e) => problems.push(format!("cannot read {}: {e}", dir.display())),
    }
    files.sort();

    let mut cases = Vec::new();
    for path in &files {
        let name = relative_name(dir, path);
        if let Some(data_name) = name.strip_suffix(SIDECAR_SUFFIX) {
            if !files.contains(&dir.join(data_name)) {
                problems.push(format!("{name}: sidecar has no data file {data_name}"));
            }
            continue;
        }
        let sidecar_file = sidecar_path(path);
        if !files.contains(&sidecar_file) {
            problems.push(format!(
                "{name}: no sidecar (expected {name}{SIDECAR_SUFFIX})"
            ));
            continue;
        }
        let bytes = match fs::read(path) {
            Ok(b) => b,
            Err(e) => {
                problems.push(format!("{name}: cannot read: {e}"));
                continue;
            }
        };
        let text = match fs::read_to_string(&sidecar_file) {
            Ok(t) => t,
            Err(e) => {
                problems.push(format!("{name}{SIDECAR_SUFFIX}: cannot read: {e}"));
                continue;
            }
        };
        match Sidecar::parse(&text) {
            Ok(sidecar) => cases.push(CorpusCase {
                name,
                path: path.clone(),
                bytes,
                sidecar,
            }),
            Err(e) => problems.push(format!("{name}{SIDECAR_SUFFIX}: {e}")),
        }
    }
    if problems.is_empty() && cases.is_empty() {
        problems.push(format!("no corpus files found in {}", dir.display()));
    }
    if problems.is_empty() {
        Ok(cases)
    } else {
        Err(CorpusError { problems })
    }
}

fn collect(dir: &Path, files: &mut Vec<PathBuf>, problems: &mut Vec<String>) {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) => {
            problems.push(format!("cannot read {}: {e}", dir.display()));
            return;
        }
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if is_hidden(&path) {
            continue;
        }
        if path.is_dir() {
            collect(&path, files, problems);
        } else {
            files.push(path);
        }
    }
}

fn is_hidden(path: &Path) -> bool {
    path.file_name()
        .is_some_and(|n| n.to_string_lossy().starts_with('.'))
}

fn relative_name(dir: &Path, path: &Path) -> String {
    let rel = path.strip_prefix(dir).unwrap_or(path);
    rel.components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

// ---- sidecar format ----------------------------------------------------------

/// The contents of a `<file>.expected.toml` sidecar.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Sidecar {
    /// What the file is for, in a sentence.
    pub description: String,
    /// The expected dialect.
    pub dialect: ExpectedDialect,
    /// The expected rows.
    pub rows: ExpectedRows,
    /// Every diagnostic kind the file should produce. A kind that is not
    /// listed must not be reported.
    #[serde(default)]
    pub diagnostics: Vec<Diagnostic>,
    /// Optional display values of chosen cells.
    #[serde(default)]
    pub cells: Vec<ExpectedCell>,
}

/// The `[dialect]` table.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExpectedDialect {
    /// `","`, `";"`, `"\t"` or `"|"`. Files with no delimiter at all (one
    /// column, or empty) expect the default, `","`.
    pub delimiter: Delimiter,
    /// The most common line ending (ties go to the first seen): `"lf"`,
    /// `"crlf"`, `"cr"`, or `"none"` if no row has one.
    #[serde(deserialize_with = "line_ending_or_none")]
    pub line_ending: Option<LineEnding>,
    /// Whether more than one kind of line ending is present.
    pub mixed_line_endings: bool,
    /// `"none"`, `"utf-8"`, `"utf-16le"` or `"utf-16be"`.
    pub bom: Bom,
    /// `"utf-8"`, `"utf-16le"`, `"utf-16be"` or `"windows-1252"`.
    pub encoding: Encoding,
    /// Whether the last row ends with a line ending.
    pub trailing_newline: bool,
    /// Whether the header-row heuristic should find a header.
    pub header: bool,
}

/// The `[rows]` table. Field counts are given either per row (`fields`) or
/// as the most common count plus exceptions (`fields_mode` and
/// `fields_exceptions`).
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExpectedRows {
    /// Number of physical rows.
    pub count: usize,
    /// The field count of every row, in order.
    #[serde(default)]
    pub fields: Option<Vec<usize>>,
    /// The field count of every row not listed in `fields_exceptions`.
    #[serde(default)]
    pub fields_mode: Option<usize>,
    /// Rows whose field count differs from `fields_mode`, by row index.
    #[serde(default)]
    pub fields_exceptions: Vec<FieldCountException>,
}

/// A row whose field count differs from `fields_mode`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FieldCountException {
    /// The row index.
    pub row: usize,
    /// Its field count.
    pub fields: usize,
}

/// An expected display value (unquoted, unescaped and decoded) for one cell.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExpectedCell {
    /// Row index.
    pub row: usize,
    /// Field index within the row.
    pub field: usize,
    /// The display value. Invalid bytes appear as U+FFFD.
    pub value: String,
    /// Whether the field is quoted, if the sidecar says.
    #[serde(default)]
    pub quoted: Option<bool>,
}

impl ExpectedRows {
    /// The field count of every row, expanded from either form.
    #[must_use]
    pub fn field_counts(&self) -> Vec<usize> {
        if let Some(fields) = &self.fields {
            return fields.clone();
        }
        let mut counts = vec![self.fields_mode.unwrap_or(0); self.count];
        for e in &self.fields_exceptions {
            if let Some(c) = counts.get_mut(e.row) {
                *c = e.fields;
            }
        }
        counts
    }
}

impl Sidecar {
    /// Parses and validates a sidecar.
    ///
    /// # Errors
    ///
    /// Returns the TOML error, or every validation problem, as one string.
    pub fn parse(text: &str) -> Result<Self, String> {
        let sidecar: Sidecar = toml::from_str(text).map_err(|e| e.to_string())?;
        sidecar.validate().map_err(|p| p.join("; "))?;
        Ok(sidecar)
    }

    /// The expected diagnostic of `kind`, if the file should have one.
    #[must_use]
    pub fn diagnostic(&self, kind: DiagnosticKind) -> Option<&Diagnostic> {
        self.diagnostics.iter().find(|d| d.kind == kind)
    }

    /// Checks that the sidecar is self-consistent (without looking at the
    /// data file).
    ///
    /// # Errors
    ///
    /// Returns every problem found.
    pub fn validate(&self) -> Result<(), Vec<String>> {
        let mut p = Vec::new();
        let rows = &self.rows;
        let d = &self.dialect;

        match (&rows.fields, rows.fields_mode) {
            (Some(fields), None) => {
                if fields.len() != rows.count {
                    p.push(format!(
                        "rows.fields has {} entries but rows.count is {}",
                        fields.len(),
                        rows.count
                    ));
                }
                if !rows.fields_exceptions.is_empty() {
                    p.push("rows.fields_exceptions needs rows.fields_mode, not rows.fields".into());
                }
            }
            (None, Some(mode)) => {
                let mut prev = None;
                for e in &rows.fields_exceptions {
                    if e.row >= rows.count {
                        p.push(format!(
                            "fields_exceptions row {} is past the last row",
                            e.row
                        ));
                    }
                    if prev.is_some_and(|r| e.row <= r) {
                        p.push(format!("fields_exceptions row {} is out of order", e.row));
                    }
                    if e.fields == mode {
                        p.push(format!(
                            "fields_exceptions row {} equals fields_mode",
                            e.row
                        ));
                    }
                    prev = Some(e.row);
                }
            }
            _ => p.push("give exactly one of rows.fields and rows.fields_mode".into()),
        }
        if rows.field_counts().contains(&0) {
            p.push("every row has at least one field".into());
        }
        if rows.count == 0 && d.trailing_newline {
            p.push("a file with no rows has no trailing newline".into());
        }
        if d.line_ending.is_none() && d.mixed_line_endings {
            p.push("mixed_line_endings needs a line_ending".into());
        }

        let mut seen = Vec::new();
        for diag in &self.diagnostics {
            let kind = diag.kind;
            if seen.contains(&kind) {
                p.push(format!("diagnostic {kind:?} is listed twice"));
            }
            seen.push(kind);
            if diag.count == 0 || diag.first.is_empty() {
                p.push(format!("{kind:?}: count and first must not be empty"));
            }
            if diag.first.len() > diag.count.min(MAX_LOCATIONS) {
                p.push(format!("{kind:?}: more locations than count"));
            }
            if diag.first.windows(2).any(|w| w[0].offset >= w[1].offset) {
                p.push(format!(
                    "{kind:?}: locations must be in increasing offset order"
                ));
            }
            let max_row = rows.count.max(1); // a BOM-only file still has row 0
            if diag.first.iter().any(|l| l.row >= max_row) {
                p.push(format!("{kind:?}: a location is past the last row"));
            }
        }
        let has = |k| seen.contains(&k);
        if (d.bom != Bom::None) != has(DiagnosticKind::BomPresent) {
            p.push("bom_present must be listed exactly when dialect.bom is not \"none\"".into());
        }
        if d.mixed_line_endings != has(DiagnosticKind::MixedLineEndings) {
            p.push(
                "mixed_line_endings must be listed exactly when dialect.mixed_line_endings".into(),
            );
        }

        let counts = rows.field_counts();
        for c in &self.cells {
            if counts.get(c.row).is_none_or(|&n| c.field >= n) {
                p.push(format!("cell {}:{} does not exist", c.row, c.field));
            }
        }

        if p.is_empty() { Ok(()) } else { Err(p) }
    }
}

fn line_ending_or_none<'de, D: Deserializer<'de>>(d: D) -> Result<Option<LineEnding>, D::Error> {
    let s = String::deserialize(d)?;
    match s.as_str() {
        "lf" => Ok(Some(LineEnding::Lf)),
        "crlf" => Ok(Some(LineEnding::Crlf)),
        "cr" => Ok(Some(LineEnding::Cr)),
        "none" => Ok(None),
        other => Err(serde::de::Error::custom(format!(
            "unknown line_ending {other:?}; expected \"lf\", \"crlf\", \"cr\" or \"none\""
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: &str = r#"
description = "Ragged."

[dialect]
delimiter = ","
line_ending = "crlf"
mixed_line_endings = false
bom = "utf-8"
encoding = "utf-8"
trailing_newline = true
header = true

[rows]
count = 3
fields_mode = 2
fields_exceptions = [{ row = 2, fields = 3 }]

[[diagnostics]]
kind = "ragged_rows"
count = 1
first = [{ row = 2, offset = 12 }]

[[diagnostics]]
kind = "bom_present"
count = 1
first = [{ row = 0, offset = 0 }]

[[cells]]
row = 1
field = 0
value = "x"
"#;

    #[test]
    fn parses_a_good_sidecar() {
        let s = Sidecar::parse(GOOD).unwrap();
        assert_eq!(s.dialect.delimiter, Delimiter::Comma);
        assert_eq!(s.dialect.line_ending, Some(LineEnding::Crlf));
        assert_eq!(s.dialect.bom, Bom::Utf8);
        assert_eq!(s.rows.field_counts(), vec![2, 2, 3]);
        assert_eq!(s.diagnostic(DiagnosticKind::RaggedRows).unwrap().count, 1);
        assert!(s.diagnostic(DiagnosticKind::NulBytes).is_none());
        assert_eq!(s.cells[0].quoted, None);
    }

    #[test]
    fn tab_and_none_spellings() {
        let text = GOOD
            .replace(r#"delimiter = ",""#, r#"delimiter = "\t""#)
            .replace(r#""crlf""#, r#""none""#);
        let s = Sidecar::parse(&text).unwrap();
        assert_eq!(s.dialect.delimiter, Delimiter::Tab);
        assert_eq!(s.dialect.line_ending, None);
    }

    #[test]
    fn rejects_bad_sidecars() {
        let cases = [
            (GOOD.replace("count = 3", "count = 2"), "past the last row"),
            (
                GOOD.replace("bom = \"utf-8\"", "bom = \"none\""),
                "bom_present",
            ),
            (
                GOOD.replace("fields = 3 }", "fields = 2 }"),
                "equals fields_mode",
            ),
            (
                GOOD.replace("fields_mode = 2", "fields = [2, 2, 3]"),
                "needs rows.fields_mode",
            ),
            (
                GOOD.replace("delimiter = \",\"", "delimiter = \":\""),
                "unknown delimiter",
            ),
            (GOOD.replace("\"crlf\"", "\"crcr\""), "unknown line_ending"),
            (
                GOOD.replace("header = true", "header = true\ncolour = 1"),
                "unknown field",
            ),
            (GOOD.replace("field = 0", "field = 5"), "does not exist"),
            (
                GOOD.replace("offset = 12", "offset = 12 }, { row = 2, offset = 3"),
                "more locations",
            ),
        ];
        for (text, needle) in cases {
            let err = Sidecar::parse(&text).expect_err(needle);
            assert!(err.contains(needle), "expected {needle:?} in {err:?}");
        }
    }

    #[test]
    fn sidecar_path_appends_the_suffix() {
        assert_eq!(
            sidecar_path(Path::new("/c/dialect/a.csv")),
            PathBuf::from("/c/dialect/a.csv.expected.toml")
        );
    }
}
