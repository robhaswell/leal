//! The serializer targets' harness: a file and an edit script, replayed on
//! a real document and on the save oracle, then saved to memory.
//!
//! An input is the file's bytes, optionally followed by [`MARKER`] and the
//! bytes of a [`Script`] (read with `arbitrary`). With no marker the whole
//! input is the file and the script is empty, so any file is a seed (F1).
//!
//! For each input:
//!
//! 1. The file is written to a temporary folder and opened as a user
//!    would, with the script's delimiter (and, for the `encodings` target,
//!    encoding) chosen; the oracle parses the same bytes with the
//!    testkit's reference parser. Every value must match before editing.
//! 2. Each operation is made on both: cell edits, row and column inserts
//!    and deletes (with values chosen to need quoting, to revert a cell,
//!    or from the file's encoding or outside it), undo and redo. They must
//!    accept and refuse the same edits.
//! 3. Saving to memory must give the oracle's bytes, splices and fixes
//!    (F2, F4, F6), or the same refusal, naming the same cells (F5).
//! 4. The saved bytes, reopened, read as the oracle's rows: every value,
//!    field count and line ending (ADR-0004 decision 10).
//! 5. Undoing every edit left and saving again gives the original bytes
//!    (F1, F3).

use std::path::PathBuf;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use arbitrary::{Arbitrary, Unstructured};
use leal_core::detect::Choices;
use leal_core::dialect::{Bom, Delimiter, Encoding};
use leal_core::document::{Document, OpenOptions};
use leal_core::edit::{Command, EditError};
use leal_core::index::RowIndex;
use leal_core::save::{MAX_NAMED_CELLS, SaveError, SaveKind};
use leal_core::schedule::{Scheduler, SchedulerConfig};
use leal_core::source::{TempFolders, VolumeInfo};
use leal_testkit::dialect::{self as tk, decode_value};
use leal_testkit::fidelity::{Change, check_identical, check_only_changed};
use leal_testkit::save::{
    Document as Oracle, Edit as OracleEdit, Fix as OracleFix, SaveError as OracleError, SavedFile,
};

use crate::{index_dialect, oracle, tk_delimiter, tk_encoding, tk_line_ending};

/// Separates the file from the edit script.
pub const MARKER: &[u8] = b"\0EDITS\0";

/// The most operations a script makes, to keep each run quick.
const MAX_OPS: usize = 24;

/// What a script does to its file.
#[derive(Arbitrary, Debug, Default)]
pub struct Script {
    /// The delimiter chosen (an index into [`Delimiter::ALL`]), or none
    /// for detection's.
    pub delimiter: Option<u8>,
    /// For the `encodings` target: the encoding chosen, an index into the
    /// encodings the file's BOM allows.
    pub encoding: u8,
    /// How it is saved.
    pub save: Save,
    /// The edits, undos and redos.
    pub ops: Vec<Op>,
}

/// How a script saves.
#[derive(Arbitrary, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Save {
    /// Save, in place.
    #[default]
    Save,
    /// Save As, in the file's encoding.
    SaveAs,
    /// Save As UTF-8 (ADR-0008 decision 7).
    SaveAsUtf8,
}

/// One step of a script. Rows and columns are taken modulo a little more
/// than the document has, so most land and some are refused.
#[derive(Arbitrary, Debug)]
pub enum Op {
    /// Set a cell's value.
    Set { row: u8, column: u8, value: Value },
    /// Insert a row of one to four values.
    InsertRow { at: u8, values: (Value, Vec<Value>) },
    /// Delete a row.
    DeleteRow { row: u8 },
    /// Insert a column.
    InsertColumn { at: u8, value: Value },
    /// Delete a column.
    DeleteColumn { column: u8 },
    /// Duplicate Row: one to three rows from `at` (task 2.5a).
    DuplicateRows { at: u8, count: u8 },
    /// Undo the last command.
    Undo,
    /// Redo the last command undone.
    Redo,
}

/// A value for an edit.
#[derive(Arbitrary, Debug)]
pub enum Value {
    /// Empty.
    Empty,
    /// The cell's original value, which removes its edit (§3.6). Outside
    /// a cell edit, empty.
    Original,
    /// Another cell's value now.
    Copy { row: u8, column: u8 },
    /// Pieces chosen to need quoting or not.
    Pieces(Vec<Piece>),
}

/// Part of a value.
#[derive(Arbitrary, Debug)]
pub enum Piece {
    /// `"`.
    Quote,
    /// The file's delimiter.
    Delimiter,
    /// Another delimiter.
    OtherDelimiter(u8),
    /// LF.
    Lf,
    /// CR.
    Cr,
    /// CRLF.
    Crlf,
    /// U+FEFF, which is a BOM at the start of a file.
    Bom,
    /// A space.
    Space,
    /// A printable ASCII character.
    Ascii(u8),
    /// The character byte `0x80 | n` is in the file's encoding (in
    /// Windows-1252 for UTF-8 and UTF-16): usually one the encoding has.
    High(u8),
    /// Any character, usually one a single-byte encoding lacks.
    Char(char),
    /// Any text.
    Text(String),
}

/// Which target the input is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    /// The encoding is detection's.
    Serialize,
    /// The encoding is the script's, from those the BOM allows: every
    /// single-byte encoding, including those detection never picks.
    Encodings,
}

/// Splits an input into its file and script.
pub fn split(data: &[u8]) -> (&[u8], Script) {
    let Some(at) = data.windows(MARKER.len()).rposition(|w| w == MARKER) else {
        return (data, Script::default());
    };
    // The ops take every byte left, so more bytes are more ops.
    let u = Unstructured::new(&data[at + MARKER.len()..]);
    let mut script = Script::arbitrary_take_rest(u).unwrap_or_default();
    script.ops.truncate(MAX_OPS);
    (&data[..at], script)
}

/// `LEAL_FUZZ_VERBOSE=1`: print what each input exercised.
static VERBOSE: LazyLock<bool> = LazyLock::new(|| std::env::var_os("LEAL_FUZZ_VERBOSE").is_some());

/// Many documents are opened, two per input: one scheduler for all.
static SCHEDULER: LazyLock<Scheduler> = LazyLock::new(|| {
    Scheduler::new(SchedulerConfig {
        background_threads: Some(2),
        ..SchedulerConfig::default()
    })
    .expect("a scheduler")
});

/// This process's temporary folder.
static DIR: LazyLock<PathBuf> = LazyLock::new(|| {
    let dir = std::env::temp_dir().join(format!("leal-fuzz-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a temporary folder");
    dir
});

/// A file in this process's temporary folder, removed when dropped.
struct TempFile(PathBuf);

impl TempFile {
    fn new(bytes: &[u8]) -> TempFile {
        static FILES: AtomicU64 = AtomicU64::new(0);
        let path = DIR.join(format!("{}.csv", FILES.fetch_add(1, Ordering::Relaxed)));
        std::fs::write(&path, bytes).expect("written");
        TempFile(path)
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Opens `bytes` with `choices`, waiting for the whole file to be indexed.
/// `None` if the choices don't fit the file (a BOM).
fn open(file: &TempFile, choices: Choices) -> Option<Document> {
    let temp = TempFolders::new(DIR.join("scratch"), DIR.join("records"));
    let options = OpenOptions {
        choices,
        first_screen_rows: 10,
        max_chars: 1000,
    };
    let (document, _) = match Document::open(
        &file.0,
        &temp,
        VolumeInfo::default(),
        &SCHEDULER,
        options,
        None,
    ) {
        Ok(opened) => opened,
        Err(leal_core::document::DocumentError::Choice(_)) => return None,
        Err(error) => panic!("open: {error:?}"),
    };
    let job = document.index_job();
    assert_eq!(
        job.control().wait_timeout(Duration::from_secs(60)),
        Some(Ok(())),
        "indexed"
    );
    Some(document)
}

/// A command made, with the oracle as it was before it.
struct Made<'a> {
    command: Command,
    edit: OracleEdit,
    before: Oracle<'a>,
}

/// Runs one input (see the module docs).
pub fn run(data: &[u8], target: Target) {
    let (bytes, script) = split(data);
    let bom = Bom::detect(bytes);
    let encoding = match target {
        Target::Serialize => None,
        Target::Encodings => {
            let allowed: Vec<Encoding> = Encoding::ALL
                .into_iter()
                .filter(|&e| bom.allows(e))
                .collect();
            Some(allowed[usize::from(script.encoding) % allowed.len()])
        }
    };
    let choices = Choices {
        delimiter: script
            .delimiter
            .map(|d| Delimiter::ALL[usize::from(d) % Delimiter::ALL.len()]),
        header: None,
        encoding,
    };
    let file = TempFile::new(bytes);
    let document = open(&file, choices).expect("the choices fit the BOM");
    let detection = document.detection();
    let delimiter = tk_delimiter(detection.delimiter);
    let encoding = tk_encoding(detection.encoding);
    let analysis = oracle::analyze(bytes, delimiter, encoding);
    let mut oracle = Oracle::new(bytes, &analysis.layout, delimiter, encoding);
    check_values(&document, &oracle, "as opened");

    // The script.
    let mut done: Vec<Made<'_>> = Vec::new();
    let mut undone: Vec<Made<'_>> = Vec::new();
    for (n, op) in script.ops.iter().enumerate() {
        let what = format!("op {n} {op:?}");
        let edit = match op {
            Op::Undo => {
                if let Some(made) = done.pop() {
                    document
                        .apply(&made.command.inverse())
                        .unwrap_or_else(|e| panic!("{what}: {e:?}"));
                    oracle = made.before.clone();
                    undone.push(made);
                }
                continue;
            }
            Op::Redo => {
                if let Some(made) = undone.pop() {
                    document
                        .apply(&made.command)
                        .unwrap_or_else(|e| panic!("{what}: {e:?}"));
                    oracle
                        .apply(&made.edit)
                        .unwrap_or_else(|e| panic!("{what}: oracle {e:?}"));
                    done.push(made);
                }
                continue;
            }
            op => oracle_edit(op, &oracle, detection.delimiter.byte(), encoding),
        };
        let before = oracle.clone();
        let expected = oracle.apply(&edit);
        let got = match &edit {
            OracleEdit::SetCell { row, column, value } => document.set_cell(*row, *column, value),
            OracleEdit::InsertRow { at, values } => {
                document.insert_rows(*at, std::slice::from_ref(values))
            }
            OracleEdit::DeleteRow { row } => document.delete_rows(*row, 1),
            OracleEdit::InsertColumn { at, value } => document.insert_column(*at, value),
            OracleEdit::DeleteColumn { column } => document.delete_column(*column),
            OracleEdit::DuplicateRows { at, count } => document.duplicate_rows(*at, *count),
        };
        match (&expected, got) {
            (Ok(()), Ok(Some(command))) => {
                done.push(Made {
                    command,
                    edit,
                    before,
                });
                undone.clear();
            }
            (Ok(()), Ok(None))
            | (
                Err(OracleError::InvalidEdit(_)),
                Err(EditError::NoSuchRow { .. } | EditError::NoSuchColumn { .. }),
            )
            | (
                Err(OracleError::AfterUnterminatedQuote(_)),
                Err(EditError::AfterUnterminatedQuote { .. }),
            ) => {}
            (_, got) => panic!("{what} = {edit:?}: oracle {expected:?}, document {got:?}"),
        }
    }

    // The save, to memory.
    let kind = match script.save {
        Save::Save => SaveKind::Save,
        Save::SaveAs => SaveKind::SaveAs,
        Save::SaveAsUtf8 => SaveKind::SaveAsUtf8,
    };
    let expected = match kind {
        SaveKind::SaveAsUtf8 => oracle.save_as_utf8(),
        SaveKind::Save | SaveKind::SaveAs => oracle.save(),
    };
    let mut out = Vec::new();
    let got = document.save_to_writer(kind, &mut out);
    if *VERBOSE {
        // What the input exercised, to check the targets reach the edits.
        eprintln!(
            "leal-fuzz: {:?} {:?}, {} ops, {} commands kept, {} undone; {kind:?}: {}",
            detection.encoding,
            detection.delimiter,
            script.ops.len(),
            done.len(),
            undone.len(),
            match &got {
                Ok(_) => "saved".to_owned(),
                Err(error) => format!("{error:?}"),
            }
        );
    }
    let saved = check_save(bytes, &document, kind, expected, got, &out);
    if let Some(saved) = saved {
        check_reopen(
            &out,
            &oracle,
            &saved,
            kind,
            detection.delimiter,
            detection.encoding,
        );
    }

    // F3: every edit undone, the file saves as it was.
    while let Some(made) = done.pop() {
        document
            .apply(&made.command.inverse())
            .unwrap_or_else(|e| panic!("undo all: {e:?}"));
    }
    // UTF-16 is read-only, and Save As UTF-8 converts.
    if kind != SaveKind::SaveAsUtf8 && detection.encoding.is_ascii_compatible() {
        let mut out = Vec::new();
        document
            .save_to_writer(kind, &mut out)
            .unwrap_or_else(|e| panic!("undo all, then save: {e:?}"));
        check_identical(bytes, &out).unwrap_or_else(|e| panic!("F3: {e}"));
    }
}

/// The oracle's edit for `op`, with its coordinates and values resolved
/// against the document as it is now.
fn oracle_edit(op: &Op, oracle: &Oracle<'_>, delimiter: u8, encoding: tk::Encoding) -> OracleEdit {
    let rows = oracle.row_count();
    let columns = oracle.max_row_len();
    // A little past the end, so some edits are refused, and hatched cells
    // (past a short row's end) are reached.
    let row = |r: u8| usize::from(r) % (rows + 2);
    let column = |c: u8| usize::from(c) % (columns + 3);
    let text = |value: &Value, at: Option<(usize, usize)>| -> String {
        match value {
            Value::Empty => String::new(),
            Value::Original => at
                .and_then(|(r, c)| oracle.original_value(r, c))
                .unwrap_or_default(),
            Value::Copy { row: r, column: c } => {
                oracle.value(row(*r), column(*c)).unwrap_or_default()
            }
            Value::Pieces(pieces) => pieces
                .iter()
                .take(16)
                .map(|piece| piece_text(piece, delimiter, encoding))
                .collect(),
        }
    };
    match op {
        Op::Set {
            row: r,
            column: c,
            value,
        } => {
            let (r, c) = (row(*r), column(*c));
            OracleEdit::SetCell {
                row: r,
                column: c,
                value: text(value, Some((r, c))),
            }
        }
        Op::InsertRow {
            at,
            values: (first, more),
        } => OracleEdit::InsertRow {
            at: row(*at),
            values: std::iter::once(first)
                .chain(more.iter().take(3))
                .map(|v| text(v, None))
                .collect(),
        },
        Op::DeleteRow { row: r } => OracleEdit::DeleteRow { row: row(*r) },
        Op::InsertColumn { at, value } => OracleEdit::InsertColumn {
            at: column(*at),
            value: text(value, None),
        },
        Op::DeleteColumn { column: c } => OracleEdit::DeleteColumn { column: column(*c) },
        Op::DuplicateRows { at, count } => OracleEdit::DuplicateRows {
            at: row(*at),
            count: 1 + usize::from(*count) % 3,
        },
        Op::Undo | Op::Redo => unreachable!("not an edit"),
    }
}

fn piece_text(piece: &Piece, delimiter: u8, encoding: tk::Encoding) -> String {
    match piece {
        Piece::Quote => "\"".into(),
        Piece::Delimiter => char::from(delimiter).into(),
        Piece::OtherDelimiter(n) => char::from(b",;\t|"[usize::from(*n) % 4]).into(),
        Piece::Lf => "\n".into(),
        Piece::Cr => "\r".into(),
        Piece::Crlf => "\r\n".into(),
        Piece::Bom => "\u{FEFF}".into(),
        Piece::Space => " ".into(),
        Piece::Ascii(b) => char::from(0x20 + b % 0x5F).into(),
        Piece::High(b) => {
            let table = if encoding.is_single_byte() {
                encoding
            } else {
                tk::Encoding::Windows1252
            };
            decode_value(&[0x80 | b], table)
        }
        Piece::Char(c) => c.to_string(),
        Piece::Text(s) => s.chars().take(8).collect(),
    }
}

/// The document's every value is the oracle's.
fn check_values(document: &Document, oracle: &Oracle<'_>, what: &str) {
    assert_eq!(
        document.row_count(),
        oracle.row_count(),
        "{what}: row count"
    );
    for row in 0..oracle.row_count() {
        let cells = document
            .cells(row..row + 1, 0..usize::MAX, usize::MAX)
            .expect("readable");
        // A row column deletes left with no cells is written `""`, so it
        // reads back as one empty field (ADR-0014 decision 6).
        let len = oracle.row_len(row).max(1);
        assert_eq!(cells[0].field_count, len, "{what}: row {row} field count");
        for column in 0..len {
            let value = document.full_value(row, column).expect("readable");
            let expected = oracle.value(row, column).or_else(|| Some(String::new()));
            assert_eq!(value, expected, "{what}: ({row}, {column})");
        }
    }
}

/// The save to memory against the oracle's; the oracle's saved file if
/// both saved.
fn check_save(
    original: &[u8],
    document: &Document,
    kind: SaveKind,
    expected: Result<SavedFile, OracleError>,
    got: Result<u64, SaveError>,
    out: &[u8],
) -> Option<SavedFile> {
    let capped = |cells: &[(usize, usize)]| {
        (
            cells[..cells.len().min(MAX_NAMED_CELLS)].to_vec(),
            cells.len() > MAX_NAMED_CELLS,
        )
    };
    match (expected, got) {
        (Ok(saved), Ok(len)) => {
            check_identical(&saved.bytes, out).unwrap_or_else(|e| panic!("{kind:?}: {e}"));
            assert_eq!(
                len,
                u64::try_from(out.len()).expect("fits"),
                "{kind:?}: length"
            );
            if kind != SaveKind::SaveAsUtf8 {
                // F2 and F6, splice by splice, and the same fixes (F4).
                check_only_changed(original, out, &saved.changes)
                    .unwrap_or_else(|e| panic!("{kind:?}: {e}"));
                let plan = document.save_plan(kind).expect("it saved");
                let splices: Vec<Change> = plan
                    .splices()
                    .iter()
                    .map(|s| Change::replace(s.range.clone(), s.bytes.clone()))
                    .collect();
                assert_eq!(splices, saved.changes, "{kind:?}: splices");
                let fixes: Vec<OracleFix> = plan
                    .fixes()
                    .iter()
                    .map(|fix| match *fix {
                        leal_core::save::Fix::EmptyRowQuoted { row } => {
                            OracleFix::EmptyRowQuoted { row }
                        }
                        leal_core::save::Fix::CrSplit { row } => OracleFix::CrSplit { row },
                        leal_core::save::Fix::BomLikeQuoted => OracleFix::BomLikeQuoted,
                    })
                    .collect();
                assert_eq!(fixes, saved.fixes, "{kind:?}: fixes");
            }
            Some(saved)
        }
        (Err(OracleError::ReadOnly), Err(SaveError::ReadOnly)) => None,
        (
            Err(OracleError::Unencodable(cells)),
            Err(SaveError::Unencodable {
                cells: named, more, ..
            }),
        ) => {
            assert_eq!(
                (named, more),
                capped(&cells),
                "{kind:?}: F5, the same cells"
            );
            None
        }
        (
            Err(OracleError::Unconvertible(cells)),
            Err(SaveError::Unconvertible {
                cells: named, more, ..
            }),
        ) => {
            assert_eq!(
                (named, more),
                capped(&cells),
                "{kind:?}: F5, the same cells"
            );
            None
        }
        (expected, got) => panic!(
            "{kind:?}: oracle {:?}, document {got:?}",
            expected.map(|s| s.bytes.escape_ascii().to_string())
        ),
    }
}

/// The saved bytes, opened afresh in the encoding they were written in,
/// read as the oracle's rows, with the line endings the oracle says.
fn check_reopen(
    out: &[u8],
    oracle: &Oracle<'_>,
    saved: &SavedFile,
    kind: SaveKind,
    delimiter: Delimiter,
    encoding: Encoding,
) {
    let file = TempFile::new(out);
    let encoding = kind.writes(encoding);
    let choices = Choices {
        delimiter: Some(delimiter),
        header: None,
        encoding: Some(encoding),
    };
    let reopened = open(&file, choices).expect("the saved BOM fits the encoding");
    check_values(&reopened, oracle, "reopened");
    let bom = Bom::detect(out);
    let index = RowIndex::build(out, index_dialect(delimiter, encoding, bom.len()))
        .expect("any bytes index");
    let endings: Vec<Option<tk::LineEnding>> = (0..index.row_count())
        .map(|r| {
            index
                .row(r, out)
                .expect("indexed")
                .line_ending
                .map(tk_line_ending)
        })
        .collect();
    assert_eq!(endings, saved.line_endings, "reopened: line endings");
}
