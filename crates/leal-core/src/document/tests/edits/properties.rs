//! The edit property (tasks 2.1, 2.4a and 2.4b): random cell edits, row
//! and column inserts and deletes, undos and redos on generated files, with
//! every reader of the document checked against the testkit's save
//! oracle's edited view (`leal_testkit::save::Document`), which never sees
//! leal-core's code.
//!
//! The edits come from the testkit's edit strategy, hatched cells and
//! edits past an unterminated quote included, runs of several rows
//! inserted or deleted at once, and columns inserted or deleted at places
//! among those there are then. A step may come after an undo the strategy
//! didn't expect, so some edits name a row or cell that isn't there, which
//! both must refuse.

use std::collections::BTreeMap;
use std::sync::LazyLock;

use leal_testkit::diagnostics::DiagnosticKind as TkKind;
use leal_testkit::save::{CellSource, Document as Oracle, Edit as OracleEdit, SaveError};
use leal_testkit::strategies::csv::{CsvConfig, GeneratedCsv};
use leal_testkit::strategies::edits::{EDIT_VALUES, EditCase, edit_case};
use proptest::collection::vec;
use proptest::prelude::*;
use proptest::sample::{Index, select};
use proptest::test_runner::TestRunner;

use super::*;
use crate::rows::{NUMBER_MAX_CHARS, NumericColumns};

/// One step of the user's: the strategy's next edit (a cell's, or a row
/// or a column inserted or deleted), a run of rows inserted or deleted at
/// once (at a place among the rows there are then), a column inserted or
/// deleted (at a place among the columns there are then), ⌘Z or ⇧⌘Z.
#[derive(Clone, Copy, Debug)]
enum Step {
    Edit,
    InsertRun(Index, usize),
    DeleteRun(Index, usize),
    InsertColumn(Index, Index),
    DeleteColumn(Index),
    Undo,
    Redo,
}

fn steps() -> impl Strategy<Value = Vec<Step>> {
    vec(
        prop_oneof![
            4 => Just(Step::Edit),
            1 => (any::<Index>(), 1..4_usize).prop_map(|(at, n)| Step::InsertRun(at, n)),
            1 => (any::<Index>(), 1..5_usize).prop_map(|(at, n)| Step::DeleteRun(at, n)),
            1 => (any::<Index>(), any::<Index>()).prop_map(|(at, v)| Step::InsertColumn(at, v)),
            1 => any::<Index>().prop_map(Step::DeleteColumn),
            2 => Just(Step::Undo),
            1 => Just(Step::Redo),
        ],
        0..20,
    )
}

/// Queries with no case folding beyond ASCII, so the oracle's expected
/// matches are a lowercase `contains`.
const QUERIES: [&str; 6] = ["a", "x", "e", "1", "\"", "new"];

/// Many documents are opened, one per case: one scheduler for all.
static SCHEDULER: LazyLock<Scheduler> = LazyLock::new(scheduler);

static DIR: LazyLock<Dir> = LazyLock::new(|| Dir::new("edit-properties"));

/// The 1.2 detection isn't under test: the file is read as the generator
/// wrote it.
fn open_case(file: &GeneratedCsv, header: bool) -> Document {
    static FILES: AtomicU64 = AtomicU64::new(0);
    let name = format!("{}.csv", FILES.fetch_add(1, Ordering::Relaxed));
    let path = DIR.file(&name, &file.bytes);
    let choices = Choices {
        delimiter: Some(Delimiter::from_byte(file.delimiter().byte()).unwrap()),
        header: Some(header),
        encoding: Encoding::ALL
            .into_iter()
            .find(|e| e.iana_name() == file.encoding.name()),
    };
    let options = OpenOptions {
        choices,
        ..options(10)
    };
    let (document, _) = Document::open(
        &path,
        &DIR.temp(),
        VolumeInfo::default(),
        &SCHEDULER,
        options,
        None,
    )
    .unwrap();
    wait_for_index(&document);
    let _ = std::fs::remove_file(path);
    document
}

/// The case's edits in the order the strategy tried them, the ones the
/// oracle refused (ADR-0004 decision 8) included at their places.
fn strategy_edits(case: &EditCase) -> Vec<OracleEdit> {
    let mut all = Vec::new();
    let mut refused = case.refused.iter().peekable();
    for (at, edit) in case.edits.iter().enumerate() {
        while let Some((_, edit)) = refused.next_if(|(place, _)| *place == at) {
            all.push(edit.clone());
        }
        all.push(edit.clone());
    }
    all.extend(refused.map(|(_, edit)| edit.clone()));
    all
}

/// A column insert (`true`) or delete at a column, in effect: for the
/// marks' most common field count (oracle rule 6).
type ColumnOp = (bool, usize);

/// The oracle and the column operations in effect.
#[derive(Clone)]
struct State<'a> {
    oracle: Oracle<'a>,
    columns: Vec<ColumnOp>,
}

/// A command made, with the state before and after it if it inserts or
/// deletes rows or a column: undo and redo of those set the oracle back as
/// it was (the oracle's own insert would make new rows, not bring back the
/// file's).
struct Done<'a> {
    command: Command,
    states: Option<(State<'a>, State<'a>)>,
}

/// The document's row or column insert or delete (`got`) against the
/// oracle's (`expected`, made from `before`): the same accepted or refused.
fn structural_step<'a>(
    expected: Result<(), SaveError>,
    got: Result<Option<Command>, EditError>,
    before: State<'a>,
    state: &mut State<'a>,
    done: &mut Vec<Done<'a>>,
    undone: &mut Vec<Done<'a>>,
) -> Result<(), TestCaseError> {
    match (expected, got) {
        (Ok(()), Ok(Some(command))) => {
            prop_assert!(command.is_structural());
            if let Edit::InsertColumn { at, .. } | Edit::DeleteColumn { at, .. } = command.edit {
                let insert = matches!(command.edit, Edit::InsertColumn { .. });
                state.columns.push((insert, at));
            }
            done.push(Done {
                command,
                states: Some((before, state.clone())),
            });
            undone.clear();
        }
        (
            Err(SaveError::InvalidEdit(_)),
            Err(EditError::NoSuchRow { .. } | EditError::NoSuchColumn { .. }),
        )
        | (
            Err(SaveError::AfterUnterminatedQuote(_)),
            Err(EditError::AfterUnterminatedQuote { .. }),
        ) => *state = before,
        (expected, got) => {
            prop_assert!(false, "oracle {:?}, document {:?}", expected, got);
        }
    }
    Ok(())
}

/// A row of `fields` fields' length after `columns` (oracle rule 6; a row
/// left with no cells is a blank line, ADR-0014 decision 6).
fn folded(fields: usize, columns: &[ColumnOp]) -> usize {
    columns
        .iter()
        .fold(fields, |len, &(insert, at)| match (insert, len) {
            (true, len) if len >= at && len > 0 => len + 1,
            (false, len) if len > at => len - 1,
            (_, len) => len,
        })
}

fn edit_of(edit: &OracleEdit) -> Option<(usize, usize, String)> {
    match edit {
        OracleEdit::SetCell { row, column, value } => Some((*row, *column, value.clone())),
        _ => None,
    }
}

/// The oracle's value of a cell: empty past the end of its row.
fn value(oracle: &Oracle<'_>, row: usize, column: usize) -> String {
    oracle.value(row, column).unwrap_or_default()
}

fn set(row: usize, column: usize, value: &str) -> OracleEdit {
    OracleEdit::SetCell {
        row,
        column,
        value: value.to_owned(),
    }
}

/// The case's cell edits in the order the strategy tried them, the ones
/// the oracle refused (ADR-0004 decision 8) included at their places.
fn cell_edits(case: &EditCase) -> Vec<(usize, usize, String)> {
    let mut all = Vec::new();
    let mut refused = case.refused.iter().peekable();
    for (at, edit) in case.edits.iter().enumerate() {
        while let Some((_, edit)) = refused.next_if(|(place, _)| *place == at) {
            all.extend(edit_of(edit));
        }
        all.extend(edit_of(edit));
    }
    all.extend(refused.filter_map(|(_, edit)| edit_of(edit)));
    all
}

/// The oracle's cell as a command sees it: `None` past the row's end.
fn cell(oracle: &Oracle<'_>, row: usize, column: usize) -> Option<String> {
    oracle.value(row, column)
}

/// Sets the oracle's cell to a command's value: a missing cell is `""` to
/// the oracle, which makes a hatched cell missing again.
fn set_oracle(oracle: &mut Oracle<'_>, change: &CellChange, value: Option<&String>) {
    let value = value.map_or("", String::as_str);
    oracle
        .apply(&set(change.row, change.column, value))
        .unwrap();
}

/// Runs `steps` on both, checking that they accept and refuse the same
/// edits, that each cell command says what the oracle's cell held before
/// and holds after, and that undo and redo of a row or column command
/// bring back the rows the oracle had.
fn run<'a>(
    document: &Document,
    state: &mut State<'a>,
    case: &EditCase,
    steps: &[Step],
) -> Result<(), TestCaseError> {
    let mut edits = strategy_edits(case).into_iter();
    let mut done: Vec<Done<'a>> = Vec::new();
    let mut undone: Vec<Done<'a>> = Vec::new();
    for step in steps {
        let before = state.clone();
        match *step {
            Step::Edit => match edits.next() {
                Some(OracleEdit::SetCell { row, column, value }) => {
                    let oracle = &mut state.oracle;
                    let new = value;
                    let old = cell(oracle, row, column);
                    let expected = oracle.apply(&set(row, column, &new));
                    let got = document.set_cell(row, column, &new);
                    match (expected, got) {
                        (Ok(()), Ok(Some(command))) => {
                            let [change] = command.changes() else {
                                panic!("one cell");
                            };
                            prop_assert_eq!((change.row, change.column), (row, column));
                            prop_assert_eq!(&change.old, &old);
                            prop_assert_eq!(&change.new, &cell(oracle, row, column));
                            done.push(Done {
                                command,
                                states: None,
                            });
                            undone.clear();
                        }
                        (Ok(()), Ok(None)) => {
                            prop_assert_eq!(
                                &old,
                                &cell(oracle, row, column),
                                "only an unchanged value"
                            );
                        }
                        (Err(SaveError::InvalidEdit(_)), Err(EditError::NoSuchRow { .. }))
                        | (
                            Err(SaveError::AfterUnterminatedQuote(_)),
                            Err(EditError::AfterUnterminatedQuote { .. }),
                        ) => {}
                        (expected, got) => {
                            prop_assert!(false, "oracle {:?}, document {:?}", expected, got);
                        }
                    }
                }
                Some(edit) => {
                    let expected = state.oracle.apply(&edit);
                    let got = match edit {
                        OracleEdit::InsertRow { at, values } => document.insert_rows(at, &[values]),
                        OracleEdit::DeleteRow { row } => document.delete_rows(row, 1),
                        OracleEdit::InsertColumn { at, value } => {
                            document.insert_column(at, &value)
                        }
                        OracleEdit::DeleteColumn { column } => document.delete_column(column),
                        OracleEdit::SetCell { .. } => unreachable!(),
                    };
                    structural_step(expected, got, before, state, &mut done, &mut undone)?;
                }
                None => {}
            },
            Step::InsertRun(at, count) => {
                let at = at.index(state.oracle.row_count() + 1);
                let rows: Vec<Vec<String>> = (0..count)
                    .map(|k| {
                        let value = EDIT_VALUES[(at + 7 * k) % EDIT_VALUES.len()];
                        vec![value.to_owned(); 1 + (at + k) % 3]
                    })
                    .collect();
                let expected = rows.iter().enumerate().try_for_each(|(k, values)| {
                    state.oracle.apply(&OracleEdit::InsertRow {
                        at: at + k,
                        values: values.clone(),
                    })
                });
                let got = document.insert_rows(at, &rows);
                structural_step(expected, got, before, state, &mut done, &mut undone)?;
            }
            Step::DeleteRun(at, count) => {
                let len = state.oracle.row_count();
                if len == 0 {
                    continue;
                }
                let at = at.index(len);
                let count = count.min(len - at);
                let expected = (0..count)
                    .try_for_each(|_| state.oracle.apply(&OracleEdit::DeleteRow { row: at }));
                let got = document.delete_rows(at, count);
                structural_step(expected, got, before, state, &mut done, &mut undone)?;
            }
            Step::InsertColumn(at, value) => {
                // Up to one past the widest row, which is refused.
                let at = at.index(state.oracle.max_row_len() + 2);
                let value = EDIT_VALUES[value.index(EDIT_VALUES.len())].to_owned();
                let expected = state.oracle.apply(&OracleEdit::InsertColumn {
                    at,
                    value: value.clone(),
                });
                let got = document.insert_column(at, &value);
                structural_step(expected, got, before, state, &mut done, &mut undone)?;
            }
            Step::DeleteColumn(column) => {
                // Up to the widest row's length, which is refused.
                let column = column.index(state.oracle.max_row_len() + 1);
                let expected = state.oracle.apply(&OracleEdit::DeleteColumn { column });
                let got = document.delete_column(column);
                structural_step(expected, got, before, state, &mut done, &mut undone)?;
            }
            Step::Undo => {
                if let Some(entry) = done.pop() {
                    document.apply(&entry.command.inverse()).unwrap();
                    match &entry.states {
                        Some((before, _)) => *state = before.clone(),
                        None => {
                            let oracle = &mut state.oracle;
                            let change = &entry.command.changes()[0];
                            set_oracle(oracle, change, change.old.as_ref());
                            prop_assert_eq!(&cell(oracle, change.row, change.column), &change.old);
                        }
                    }
                    undone.push(entry);
                }
            }
            Step::Redo => {
                if let Some(entry) = undone.pop() {
                    document.apply(&entry.command).unwrap();
                    match &entry.states {
                        Some((_, after)) => *state = after.clone(),
                        None => {
                            let oracle = &mut state.oracle;
                            let change = &entry.command.changes()[0];
                            set_oracle(oracle, change, change.new.as_ref());
                            prop_assert_eq!(&cell(oracle, change.row, change.column), &change.new);
                        }
                    }
                    done.push(entry);
                }
            }
        }
        check_sources(document, &state.oracle, step)?;
    }
    Ok(())
}

/// After every step: each cell's identity and what it reads as against
/// where the oracle says it comes from (an original field, by index; an
/// edited or new value; padding), each row of the file's `same_shape`
/// against the oracle's, and the edited cells counted (not those a column
/// delete hides).
fn check_sources(
    document: &Document,
    oracle: &Oracle<'_>,
    step: &Step,
) -> Result<(), TestCaseError> {
    use crate::document::view::ViewCell;
    use crate::edit::CellId;
    let reading = document.current();
    let rows = document.row_count();
    prop_assert_eq!(rows, oracle.row_count());
    let got = Document::read_rows_of(&reading, 0..rows, |view| {
        let cells: Vec<(Option<CellId>, Option<CellSource>)> = (0..view.len())
            .map(|c| {
                let id = view.cell_id(c);
                let source = match (id, view.cell(c)) {
                    (Some(CellId::Field(k)), Some(ViewCell::Field(_))) => {
                        Some(CellSource::Original { field: k as usize })
                    }
                    (_, Some(ViewCell::Edited(_) | ViewCell::New(_))) => Some(CellSource::Edited),
                    (_, Some(ViewCell::Padding)) => Some(CellSource::Padding),
                    (_, Some(ViewCell::Field(_)) | None) => None,
                };
                (id, source)
            })
            .collect();
        let edited = (0..view.len())
            .filter(|&c| matches!(view.cell(c), Some(ViewCell::Edited(_))))
            .count();
        (cells, view.same_shape(), edited)
    })
    .unwrap();
    let mut edited = 0;
    for (row, (cells, same_shape, row_edited)) in got.iter().enumerate() {
        let expected: Vec<Option<CellSource>> = (0..oracle.row_len(row))
            .map(|c| oracle.cell_source(row, c))
            .collect();
        let sources: Vec<Option<CellSource>> = cells.iter().map(|&(_, source)| source).collect();
        prop_assert_eq!(
            &sources,
            &expected,
            "row {} ids {:?} after {:?}",
            row,
            cells,
            step
        );
        if let Some(expected) = oracle.same_shape(row) {
            prop_assert_eq!(
                *same_shape,
                expected,
                "row {} ids {:?} after {:?}",
                row,
                cells,
                step
            );
        }
        edited += row_edited;
    }
    prop_assert_eq!(document.edited_cells(), edited, "after {:?}", step);
    Ok(())
}

/// Every cell's value as the oracle has it now.
fn oracle_values(oracle: &Oracle<'_>) -> Vec<Vec<String>> {
    (0..oracle.row_count())
        .map(|r| {
            (0..oracle.row_len(r))
                .map(|c| value(oracle, r, c))
                .collect()
        })
        .collect()
}

/// The first `max` characters of `text`, and whether it has more.
fn prefix(text: &str, max: usize) -> (&str, bool) {
    match text.char_indices().nth(max) {
        Some((cut, _)) => (&text[..cut], true),
        None => (text, false),
    }
}

/// The testkit's kinds the row marks flag.
fn flagged_kind(kind: TkKind) -> Option<DiagnosticKind> {
    match kind {
        TkKind::UnterminatedQuote => Some(DiagnosticKind::UnterminatedQuote),
        TkKind::TextAfterClosingQuote => Some(DiagnosticKind::TextAfterClosingQuote),
        TkKind::InvalidEncoding => Some(DiagnosticKind::InvalidEncoding),
        TkKind::NulBytes => Some(DiagnosticKind::NulBytes),
        _ => None,
    }
}

/// What the marks must say as the oracle's cells read now: each original
/// field's kinds from the testkit's own diagnostics, each edited value on
/// the value (only a NUL can be in one), and ragged against the file's
/// most common field count, as a row of that many fields reads after the
/// column inserts and deletes in effect (the column count).
struct Expected {
    flags: Vec<RowFlags>,
    places: BTreeMap<DiagnosticKind, Vec<Place>>,
}

fn expected_marks(file: &GeneratedCsv, state: &State<'_>) -> Option<Expected> {
    let oracle = &state.oracle;
    let mut field_kinds: BTreeMap<(usize, usize), Vec<DiagnosticKind>> = BTreeMap::new();
    for diagnostic in &file.diagnostics {
        let Some(kind) = flagged_kind(diagnostic.kind) else {
            continue;
        };
        if diagnostic.first.len() < diagnostic.count {
            return None; // not every location is listed
        }
        for location in &diagnostic.first {
            let field = file.layout.field_of_offset(location.offset)?;
            field_kinds.entry(field).or_default().push(kind);
        }
    }
    let mode = file
        .layout
        .field_count_mode()
        .map(|mode| folded(mode, &state.columns));
    let mut flags = Vec::new();
    let mut places: BTreeMap<DiagnosticKind, Vec<Place>> = BTreeMap::new();
    for row in 0..oracle.row_count() {
        let cells = oracle.row_len(row);
        let source = oracle.source_row(row);
        let mut found: Vec<(DiagnosticKind, usize)> = Vec::new();
        for column in 0..cells {
            let kinds = match oracle.cell_source(row, column) {
                Some(CellSource::Original { field }) => source
                    .and_then(|s| field_kinds.get(&(s, field)))
                    .cloned()
                    .unwrap_or_default(),
                Some(CellSource::Edited) if value(oracle, row, column).contains('\0') => {
                    vec![DiagnosticKind::NulBytes]
                }
                _ => Vec::new(),
            };
            for kind in kinds {
                if !found.iter().any(|&(k, _)| k == kind) {
                    found.push((kind, column));
                }
            }
        }
        let blank = cells == 1
            && oracle.cell_source(row, 0) == Some(CellSource::Original { field: 0 })
            && source.is_some_and(|s| file.layout.rows[s].is_blank());
        let ragged = !blank && mode.is_some_and(|mode| cells != mode);
        flags.push(RowFlags {
            marked: ragged || !found.is_empty(),
            ragged,
        });
        if ragged {
            let column = mode.map_or(cells, |mode| cells.min(mode));
            places
                .entry(DiagnosticKind::RaggedRows)
                .or_default()
                .push(Place { row, column });
        }
        for (kind, column) in found {
            places.entry(kind).or_default().push(Place { row, column });
        }
    }
    Some(Expected { flags, places })
}

/// Every match of `query` among the oracle's cells, after the header row.
fn expected_matches(values: &[Vec<String>], header: bool, query: &str) -> Vec<Place> {
    let mut places = Vec::new();
    for (row, cells) in values.iter().enumerate().skip(usize::from(header)) {
        for (column, text) in cells.iter().enumerate() {
            if text.to_lowercase().contains(query) {
                places.push(Place { row, column });
            }
        }
    }
    places
}

/// Every reader of `document` against the oracle.
fn check_readers(
    document: &Document,
    state: &State<'_>,
    file: &GeneratedCsv,
    header: bool,
    query: &str,
    early: &Search,
) -> Result<(), TestCaseError> {
    let oracle = &state.oracle;
    let values = oracle_values(oracle);
    let rows = values.len();
    prop_assert_eq!(document.row_count(), rows);
    let mode = file.layout.field_count_mode();
    let columns = if rows == 0 {
        0
    } else {
        mode.map_or(0, |mode| folded(mode, &state.columns))
    };
    prop_assert_eq!(document.column_count(), columns);
    let widest = values.iter().map(Vec::len).max().unwrap_or(0);

    // The grid, and a window of it.
    let got: Vec<Vec<String>> = document
        .rows(0..rows, 100_000)
        .unwrap()
        .into_iter()
        .map(|row| row.into_iter().map(|cell| cell.text).collect())
        .collect();
    prop_assert_eq!(&got, &values);
    let window = document.cells(0..rows, 1..3, 2).unwrap();
    for (row, cells) in window.iter().zip(&values) {
        prop_assert_eq!(row.field_count, cells.len());
        let expected: Vec<Cell> = cells
            .iter()
            .skip(1)
            .take(2)
            .map(|text| {
                let (text, truncated) = prefix(text, 2);
                Cell {
                    text: text.to_owned(),
                    truncated,
                }
            })
            .collect();
        prop_assert_eq!(&row.cells, &expected);
    }

    // The inspector and the editor, cell by cell: on a large file, the
    // edited rows and every 50th (the readers above cover every row).
    let edited = |row: usize| {
        oracle.source_row(row).is_none_or(|source| {
            oracle.row_len(row) != file.layout.rows[source].fields.len()
                || (0..oracle.row_len(row)).any(|column| {
                    oracle.cell_source(row, column) != Some(CellSource::Original { field: column })
                })
        })
    };
    let sampled = |row: usize| rows <= 200 || row.is_multiple_of(50) || edited(row);
    for (row, cells) in values.iter().enumerate().filter(|&(row, _)| sampled(row)) {
        for column in 0..=cells.len() {
            let text = cells.get(column).cloned().unwrap_or_default();
            let full = document.full_value(row, column).unwrap();
            prop_assert_eq!(full.as_deref(), Some(text.as_str()));
            let shown = document.cell_value(row, column, 3).unwrap().unwrap();
            prop_assert_eq!(shown.exists, column < cells.len());
            prop_assert_eq!(shown.characters, text.chars().count());
            prop_assert_eq!(&shown.text, prefix(&text, 3).0);
        }
    }

    // Copy.
    let mut tsv = String::new();
    for (row, cells) in values.iter().enumerate() {
        if row > 0 {
            tsv.push('\n');
        }
        for column in 0..=widest {
            if column > 0 {
                tsv.push('\t');
            }
            if let Some(text) = cells.get(column) {
                push_tsv_cell(&mut tsv, text);
            }
        }
    }
    let copied = document.copy_cells_now(0..rows, 0..widest + 1).unwrap();
    prop_assert_eq!(copied.as_deref(), Some(tsv.as_str()));
    let job = document.copy_cells(0..rows, 0..widest + 1);
    prop_assert_eq!(job.control().wait_timeout(LONG), Some(Ok(())));
    prop_assert_eq!(job.wait().unwrap().take(), Some(tsv));

    // Find: one started before the edits, which kept up, and a new one.
    let expected: Vec<(Place, u64)> = expected_matches(&values, header, query)
        .into_iter()
        .zip(1..)
        .collect();
    let fresh = search(document, query);
    for found in [early, &fresh] {
        // A search started before a column insert or delete starts again
        // (ADR-0014 decision 2): once its own job has finished, a catch-up
        // job searches the rows again.
        while found.progress().catching_up {
            if let Some(job) = found.catch_up_job() {
                let _ = job.control().wait_timeout(LONG);
            }
        }
        prop_assert_eq!(found.progress().matches, expected.len() as u64);
        prop_assert_eq!(&matches(found), &expected);
        let mut highlights: Vec<Place> = found
            .matches_in(0..rows, 0..widest + 1, 100)
            .unwrap()
            .iter()
            .map(|m| Place {
                row: m.row,
                column: m.column,
            })
            .collect();
        highlights.sort_unstable_by_key(|p| (p.row, p.column));
        let places: Vec<Place> = expected.iter().map(|&(p, _)| p).collect();
        prop_assert_eq!(highlights, places);
    }

    // The diagnostics marks, and each kind's Previous and Next.
    if let Some(marks) = expected_marks(file, state) {
        prop_assert_eq!(&document.row_flags(0..rows), &marks.flags);
        let marked: Vec<usize> = (0..rows).filter(|&r| marks.flags[r].marked).collect();
        let mut next = Vec::new();
        let mut from = 0;
        while let Some(row) = document.next_row_with_diagnostic(from) {
            next.push(row);
            from = row + 1;
        }
        prop_assert_eq!(&next, &marked);
        let mut previous = Vec::new();
        let mut to = rows;
        while let Some(row) = document.previous_row_with_diagnostic(to) {
            previous.push(row);
            to = row;
        }
        previous.reverse();
        prop_assert_eq!(&previous, &marked);
        for row in 0..rows {
            prop_assert_eq!(document.row_has_diagnostic(row), marks.flags[row].marked);
        }
        for kind in [
            DiagnosticKind::UnterminatedQuote,
            DiagnosticKind::RaggedRows,
            DiagnosticKind::TextAfterClosingQuote,
            DiagnosticKind::InvalidEncoding,
            DiagnosticKind::NulBytes,
        ] {
            let places = marks.places.get(&kind).cloned().unwrap_or_default();
            prop_assert_eq!(&walk_forward(document, kind), &places, "{:?}", kind);
            prop_assert_eq!(&walk_backward(document, kind), &places, "{:?}", kind);
        }
    }

    // Number detection.
    let mut numbers = NumericColumns::new();
    for cells in values.iter().skip(usize::from(header)) {
        for (column, text) in cells.iter().enumerate() {
            let (text, truncated) = prefix(text, NUMBER_MAX_CHARS);
            numbers.add(column, text, truncated);
        }
    }
    prop_assert_eq!(document.numeric_columns(rows).unwrap(), numbers.result());
    Ok(())
}

/// Runs `test` on values from `strategy` a `divisor`th as many times as
/// usual (`PROPTEST_CASES`, 256 by default). The `proptest!` macro would let
/// `PROPTEST_CASES` override a smaller count, so the runner is built by hand,
/// as in `tests/detect_properties.rs`. It names this file, as the macro
/// does, so failures are saved to and replayed from
/// `proptest-regressions/document/tests/edits/properties.txt`.
fn run_scaled<S: Strategy>(
    divisor: u32,
    strategy: &S,
    test: impl Fn(S::Value) -> Result<(), TestCaseError>,
) {
    let config = ProptestConfig::default(); // reads PROPTEST_CASES
    let mut runner = TestRunner::new(ProptestConfig {
        cases: config.cases.div_ceil(divisor),
        source_file: Some(file!()),
        ..config
    });
    if let Err(error) = runner.run(strategy, test) {
        panic!("{error}");
    }
}

/// Each case opens a document and runs several searches and copies, so
/// the small-file properties run a quarter of the usual number (64 by
/// default, 5,000 under `just test-deep`).
const SMALL: u32 = 4;

/// [`every_reader_agrees_with_the_oracle_after_edits_undo_and_redo`]'s
/// body, for any files.
fn every_reader_agrees(
    (case, steps, header, query): (EditCase, Vec<Step>, bool, &str),
) -> Result<(), TestCaseError> {
    let document = open_case(&case.file, header);
    let mut state = State {
        oracle: case.file.document(),
        columns: Vec::new(),
    };
    let early = search(&document, query);
    run(&document, &mut state, &case, &steps)?;
    check_readers(&document, &state, &case.file, header, query, &early)?;
    // With the file's rows and columns as they were, the document has
    // unsaved edits exactly when saving would change the file's bytes.
    // With rows or columns inserted or deleted, it has them even if what
    // was inserted reads as what was deleted did.
    let oracle = &state.oracle;
    let rows_as_read = oracle.row_count() == case.file.layout.rows.len()
        && (0..oracle.row_count()).all(|row| oracle.source_row(row) == Some(row))
        && state.columns.is_empty();
    if !rows_as_read {
        prop_assert!(document.has_edits());
        return Ok(());
    }
    match oracle.save() {
        Ok(saved) => prop_assert_eq!(document.has_edits(), saved.bytes != case.file.bytes),
        Err(SaveError::Unencodable(_)) => prop_assert!(document.has_edits()),
        Err(error) => prop_assert!(false, "{}", error),
    }
    Ok(())
}

/// Every reader agrees with the oracle's edited view after any cell edits
/// (the oracle's refused ones included), undos and redos, hatched cells and
/// the header row included.
#[test]
fn every_reader_agrees_with_the_oracle_after_edits_undo_and_redo() {
    let strategy = (
        edit_case(CsvConfig::messy()),
        steps(),
        any::<bool>(),
        select(&QUERIES[..]),
    );
    run_scaled(SMALL, &strategy, every_reader_agrees);
}

/// Files of up to 3,000 rows of up to 8 fields: often past the first 64 KB
/// and over several search chunks (64 KB) and copy chunks (1,024 rows).
fn larger() -> CsvConfig {
    CsvConfig {
        max_rows: 3_000,
        max_fields: 8,
        max_value_chunks: 8,
        ..CsvConfig::messy()
    }
}

/// [`every_reader_agrees_with_the_oracle_after_edits_undo_and_redo`] on
/// larger files. A case takes about 0.4 s (mostly generating it), so a
/// 128th of the usual number: 2 by default, 157 under `just test-deep`
/// (about 2 minutes), 782 in the nightly run (about 9 minutes here, perhaps
/// 20 on CI, well inside its 45-minute limit).
#[test]
fn every_reader_agrees_with_the_oracle_on_larger_files() {
    let strategy = (
        edit_case(larger()),
        steps(),
        any::<bool>(),
        select(&QUERIES[..]),
    );
    run_scaled(128, &strategy, every_reader_agrees);
}

/// Replaying the commands into a freshly opened document of the same file
/// gives the same cells, and applies every command (ADR-0008 decision 5):
/// row inserts and deletes by value (ADR-0014 decision 3).
#[test]
fn replaying_the_history_into_a_fresh_document_gives_the_same_cells() {
    let strategy = (
        edit_case(CsvConfig::messy()),
        any::<bool>(),
        vec(0..4_u8, 0..40),
    );
    run_scaled(SMALL, &strategy, |(case, header, undos)| {
        let document = open_case(&case.file, header);
        let mut history = Vec::new();
        let mut undos = undos.into_iter();
        for edit in strategy_edits(&case) {
            let made = match edit {
                OracleEdit::SetCell { row, column, value } => {
                    document.set_cell(row, column, &value)
                }
                OracleEdit::InsertRow { at, values } => document.insert_rows(at, &[values]),
                OracleEdit::DeleteRow { row } => document.delete_rows(row, 1),
                OracleEdit::InsertColumn { at, value } => document.insert_column(at, &value),
                OracleEdit::DeleteColumn { column } => document.delete_column(column),
            };
            let Ok(Some(command)) = made else { continue };
            history.push(command.clone());
            // Sometimes undone at once (the journal has the inverse, which
            // a replay applies by value), and sometimes redone after.
            if command.is_structural() && undos.next().unwrap_or(0) > 1 {
                document.apply(&command.inverse()).unwrap();
                history.push(command.inverse());
                if undos.next().unwrap_or(0) > 1 {
                    document.apply(&command).unwrap();
                    history.push(command);
                }
            }
        }
        let fresh = open_case(&case.file, header);
        let replay = fresh.replay(&history);
        prop_assert_eq!(replay.commands.len(), history.len());
        prop_assert!(replay.refused.is_empty());
        let rows = document.row_count();
        prop_assert_eq!(fresh.row_count(), rows);
        // One way only: a row delete undone by value comes back as an
        // inserted row, and an inserted row can't have a missing cell, so
        // the replay may carry trailing empty fields where the original
        // had missing cells (ADR-0014 decision 3, ADR-0012 decision 4).
        // Anything else, a lost trailing field included, is a failure.
        let cells = |document: &Document| -> Vec<Vec<String>> {
            let rows = document.rows(0..rows, 1000).unwrap().into_iter();
            rows.map(|row| row.into_iter().map(|cell| cell.text).collect())
                .collect()
        };
        for (fresh, doc) in cells(&fresh).iter().zip(cells(&document)) {
            prop_assert!(fresh.len() >= doc.len());
            prop_assert_eq!(&fresh[..doc.len()], &doc[..]);
            prop_assert!(fresh[doc.len()..].iter().all(String::is_empty));
        }
        // Undoing the replayed history in reverse order leaves no edits.
        for command in replay.commands.iter().rev() {
            fresh.apply(&command.inverse()).unwrap();
        }
        prop_assert!(!fresh.has_edits());
        Ok(())
    });
}

/// The same cell edits as one batch (a paste) read as the oracle has them
/// applied one by one, and the batch's inverse leaves no edits.
#[test]
fn a_batch_of_edits_agrees_with_the_oracle() {
    let strategy = (edit_case(CsvConfig::messy()), any::<bool>());
    run_scaled(SMALL, &strategy, |(case, header)| {
        let mut oracle = case.file.document();
        let accepted: Vec<(usize, usize, String)> = cell_edits(&case)
            .into_iter()
            .filter(|(row, column, value)| oracle.apply(&set(*row, *column, value)).is_ok())
            .collect();
        let document = open_case(&case.file, header);
        let cells: Vec<(usize, usize, &str)> = accepted
            .iter()
            .map(|(row, column, value)| (*row, *column, value.as_str()))
            .collect();
        let batch = document.set_cells(&cells).unwrap();
        let got: Vec<Vec<String>> = document
            .rows(0..oracle.row_count(), 100_000)
            .unwrap()
            .into_iter()
            .map(|row| row.into_iter().map(|cell| cell.text).collect())
            .collect();
        prop_assert_eq!(got, oracle_values(&oracle));
        if let Some(batch) = batch {
            document.apply(&batch.inverse()).unwrap();
        }
        prop_assert!(!document.has_edits());
        Ok(())
    });
}
