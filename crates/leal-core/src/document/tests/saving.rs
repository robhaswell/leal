//! Saving (task 2.2): the splice writer on real files, the save job's
//! cancellation, the check before writing, the metadata a save keeps, the
//! rebase onto the saved file (ADR-0008 decision 1), and Save As from an
//! incomplete document (ADR-0008 decision 6). The properties against the
//! save oracle are in `saving/properties.rs`.

use super::*;

use std::os::unix::fs::PermissionsExt;
use std::sync::mpsc;

use leal_testkit::fidelity::{Change, assert_identical, assert_only_changed};

use crate::attributes::{Fingerprint, Interpretation};
use crate::edit::Command;
use crate::save::Placed;
use crate::save::{Fix, SaveError, SaveKind, SaveRequest, Saved};
use crate::source::tests::{attribute, write_attribute};
use crate::source::{
    INTERPRETATION_ATTRIBUTE_C, SimulatedShare, SimulatedShareFailure, TEXT_ENCODING_ATTRIBUTE_C,
};

mod properties;

/// Retry waits short enough for tests.
const QUICK_RETRIES: &[Duration] = &[Duration::from_millis(1); 5];

/// Opens the file at `path`, indexed, shared for saving.
fn open_at(path: &Path, dir: &Dir, scheduler: &Scheduler) -> Arc<Document> {
    let (document, _) = Document::open(
        path,
        &dir.temp(),
        VolumeInfo::default(),
        scheduler,
        options(30),
        None,
    )
    .unwrap();
    wait_for_index(&document);
    Arc::new(document)
}

fn save(document: &Arc<Document>, path: &Path, kind: SaveKind) -> Result<Saved, String> {
    let job = document.save(SaveRequest::new(path, kind));
    job.wait().cloned().map_err(|error| format!("{error:?}"))
}

fn set(document: &Document, row: usize, column: usize, value: &str) -> Command {
    document.set_cell(row, column, value).unwrap().unwrap()
}

/// What is left in `dir` besides `keep`: a save leaves nothing of its own.
fn leftovers(dir: &Path, keep: &[&str]) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| !keep.contains(&name.as_str()))
        .collect();
    names.sort();
    names
}

// ---------------------------------------------------------------------------
// The splice writer

/// F1: Save As with no edits writes a byte-identical file, and Save does
/// too.
#[test]
fn a_save_with_no_edits_writes_the_same_bytes() {
    let dir = Dir::new("save-f1");
    let bytes = sample(300 * 1024);
    let path = dir.file("a.csv", &bytes);
    let scheduler = scheduler();
    let document = open_at(&path, &dir, &scheduler);
    let copy = dir.0.join("copy.csv");
    let saved = save(&document, &copy, SaveKind::SaveAs).unwrap();
    assert_identical(&bytes, &std::fs::read(&copy).unwrap());
    assert_eq!((saved.len, saved.complete), (bytes.len() as u64, true));
    wait_for_index(&document);
    assert_eq!(saved.rows, document.row_count());
    assert_eq!(
        document.original().path,
        copy,
        "the document is the copy now"
    );
    save(&document, &copy, SaveKind::Save).unwrap();
    assert_identical(&bytes, &std::fs::read(&copy).unwrap());
    assert_eq!(
        leftovers(&dir.0, &["a.csv", "copy.csv", "scratch", "records"]),
        [""; 0]
    );
}

/// F2: an edit changes only its field's bytes; the rest is copied, past
/// several write chunks.
#[test]
fn an_edit_is_spliced_in_and_nothing_else_changes() {
    let dir = Dir::new("save-f2");
    let bytes = sample(3 * SAVE_CHUNK_BYTES);
    let path = dir.file("a.csv", &bytes);
    let scheduler = scheduler();
    let document = open_at(&path, &dir, &scheduler);
    let row = document.row_count() - 2;
    set(&document, 1, 1, "first, edited");
    set(&document, row, 0, "x");
    let plan = document.save_plan(SaveKind::Save).unwrap();
    assert_eq!(plan.splices().len(), 2);
    let saved = save(&document, &path, SaveKind::Save).unwrap();
    let out = std::fs::read(&path).unwrap();
    let changes: Vec<Change> = plan
        .splices()
        .iter()
        .map(|s| Change::replace(s.range.clone(), s.bytes.clone()))
        .collect();
    assert_only_changed(&bytes, &out, &changes);
    assert_eq!(&plan.splices()[0].bytes, b"\"first, edited\"");
    assert_eq!(saved.len, out.len() as u64);
    assert!(saved.fixes.is_empty());
}

/// ADR-0005 decision 2: a hatched cell gets the delimiters it needs and its
/// value at the end of its row, before the line ending; in a file that
/// quotes every field, quoted. A blank line edited in column 2 becomes a
/// row of three fields.
#[test]
fn a_hatched_cell_is_appended_before_the_line_ending() {
    let dir = Dir::new("save-hatched");
    let scheduler = scheduler();
    for (bytes, edits, expected) in [
        (
            &b"a,b,c\r\nd\r\n\r\ne,f,g\r\n"[..],
            &[(1, 2, "x"), (2, 2, "y")][..],
            &b"a,b,c\r\nd,,x\r\n,,y\r\ne,f,g\r\n"[..],
        ),
        (
            b"\"a\",\"b\"\n\"c\"\n",
            &[(1, 1, "d")],
            b"\"a\",\"b\"\n\"c\",\"d\"\n",
        ),
        (b"a,b\nc", &[(1, 3, "q,r")], b"a,b\nc,,,\"q,r\""),
    ] {
        let path = dir.file("h.csv", bytes);
        let document = open_at(&path, &dir, &scheduler);
        for &(row, column, value) in edits {
            set(&document, row, column, value);
        }
        save(&document, &path, SaveKind::Save).unwrap();
        assert_eq!(
            std::fs::read(&path).unwrap().escape_ascii().to_string(),
            expected.escape_ascii().to_string()
        );
    }
}

/// ADR-0004 decisions 6, 7 and 10: an emptied one-field row is written
/// A fix that rewrites the whole row of an unterminated quote keeps the
/// quote open, and the rebased reading's index has it where it now is
/// (found by the save property, deep run: `,"` with its first cell set to
/// U+FEFF and `x`).
#[test]
fn a_whole_row_fix_keeps_an_open_quote_open_where_it_moved() {
    let dir = Dir::new("save-open-quote-fix");
    let scheduler = scheduler();
    let path = dir.file("a.csv", b",\"");
    let document = open_at(&path, &dir, &scheduler);
    set(&document, 0, 0, "\u{feff}x");
    let saved = save(&document, &path, SaveKind::Save).unwrap();
    assert_eq!(saved.fixes, [Fix::BomLikeQuoted]);
    let out = std::fs::read(&path).unwrap();
    assert_eq!(out, b"\"\xef\xbb\xbfx\",\"");
    let reading = document.current();
    assert_eq!(reading.head_index.unterminated_quote(), Some(7));
    let built = RowIndex::build(&out, reading.parser.dialect()).unwrap();
    assert_eq!(built.unterminated_quote(), Some(7));
}

/// `""`, and a first field starting with U+FEFF in a file without a BOM is
/// quoted; each as one splice of the whole row.
#[test]
fn the_fixes_keep_a_reopen_reading_the_same_rows() {
    let dir = Dir::new("save-fixes");
    let scheduler = scheduler();
    let path = dir.file("f.csv", b"a\nb\nc\n");
    let document = open_at(&path, &dir, &scheduler);
    set(&document, 1, 0, "");
    set(&document, 0, 0, "\u{FEFF}x");
    let plan = document.save_plan(SaveKind::Save).unwrap();
    assert_eq!(
        plan.fixes(),
        [Fix::EmptyRowQuoted { row: 1 }, Fix::BomLikeQuoted]
    );
    assert_eq!(plan.splices()[0].range, 0..2);
    assert_eq!(plan.splices()[1].range, 2..4);
    save(&document, &path, SaveKind::Save).unwrap();
    assert_eq!(
        std::fs::read(&path).unwrap(),
        "\"\u{FEFF}x\"\n\"\"\nc\n".as_bytes()
    );
}

/// Until task 2.3, a single-byte file takes ASCII edits only; the rest is
/// refused, naming the cells, with the file untouched. UTF-16 files are
/// read-only.
#[test]
fn single_byte_and_utf16_files_are_saved_or_refused() {
    let dir = Dir::new("save-encodings");
    let scheduler = scheduler();
    let bytes = b"caf\xE9,x\nna\xEFve,y\n";
    let path = dir.file("w.csv", bytes);
    let document = open_at(&path, &dir, &scheduler);
    assert_eq!(document.detection().encoding, Encoding::Windows1252);
    set(&document, 1, 1, "plain");
    let saved = save(&document, &path, SaveKind::Save).unwrap();
    assert_eq!(
        std::fs::read(&path).unwrap(),
        b"caf\xE9,x\nna\xEFve,plain\n"
    );
    assert_eq!(saved.attributes.text_encoding, None, "the guess holds");
    set(&document, 0, 1, "\u{e9}");
    let error = save(&document, &path, SaveKind::Save).unwrap_err();
    assert!(error.contains("EncodingNotSupported"), "{error}");
    assert!(error.contains("[(0, 1)]"), "{error}");
    assert_eq!(
        std::fs::read(&path).unwrap(),
        b"caf\xE9,x\nna\xEFve,plain\n"
    );

    let utf16 = dir.file("u.csv", b"\xFF\xFEa\0,\0b\0\n\0");
    let document = open_at(&utf16, &dir, &scheduler);
    assert!(matches!(
        document.save_plan(SaveKind::SaveAs),
        Err(SaveError::ReadOnly)
    ));
}

// ---------------------------------------------------------------------------
// The job

/// A save cancelled part-way (held at its fourth checkpoint, then
/// cancelled) leaves the user's file as it was, and removes what it wrote:
/// the new file, its folder and its record.
#[test]
fn a_cancelled_save_leaves_the_file_and_removes_what_it_wrote() {
    let dir = Dir::new("save-cancel");
    let bytes = sample(6 * SAVE_CHUNK_BYTES);
    let path = dir.file("a.csv", &bytes);
    let scheduler = scheduler();
    let document = open_at(&path, &dir, &scheduler);
    set(&document, 3, 1, "edited");
    let (reached, held) = (mpsc::channel(), mpsc::channel::<()>());
    let (tell, go) = (Mutex::new(reached.0), Mutex::new(held.1));
    let hook: crate::document::saving::ChunkHook = Arc::new(move |checkpoint| {
        if checkpoint == 3 {
            tell.lock().unwrap().send(()).unwrap();
            go.lock().unwrap().recv().unwrap();
        }
    });
    let job = document.save_hooked(SaveRequest::new(&path, SaveKind::Save), hook);
    reached.1.recv_timeout(LONG).unwrap();
    let written = job.progress();
    assert!(
        written.written > 0 && written.written < written.total,
        "{written:?}"
    );
    let staging = leftovers(&dir.0, &["a.csv", "scratch", "records"]);
    assert_eq!(staging.len(), 1, "the new file's folder: {staging:?}");
    job.cancel();
    held.0.send(()).unwrap();
    assert!(matches!(job.wait(), Err(SaveError::Cancelled)));
    assert_identical(&bytes, &std::fs::read(&path).unwrap());
    assert_eq!(leftovers(&dir.0, &["a.csv", "scratch", "records"]), [""; 0]);
    let records = std::fs::read_dir(dir.0.join("records")).unwrap().count();
    assert_eq!(records, 1, "only the document's own clone is recorded");
    // The document is as it was, edits and all, and saves.
    assert!(document.has_edits());
    assert!(!document.original().diverged);
    save(&document, &path, SaveKind::Save).unwrap();
    assert!(!document.has_edits());
}

// ---------------------------------------------------------------------------
// The check before writing (ADR-0008 decision 9)

/// A file changed elsewhere is refused with its own error, unless the user
/// agrees; one that isn't there any more is refused too.
#[test]
fn a_file_changed_elsewhere_is_refused_unless_the_user_agrees() {
    let dir = Dir::new("save-check");
    let scheduler = scheduler();
    let path = dir.file("a.csv", b"a,b\n1,2\n");
    let document = open_at(&path, &dir, &scheduler);
    set(&document, 1, 0, "x");
    // Another app appends a row; no watcher is running, so only the check
    // before writing can see it.
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap();
    std::io::Write::write_all(&mut file, b"3,4\n").unwrap();
    drop(file);
    let error = save(&document, &path, SaveKind::Save).unwrap_err();
    assert_eq!(error, "ChangedElsewhere");
    assert_eq!(std::fs::read(&path).unwrap(), b"a,b\n1,2\n3,4\n");
    let mut request = SaveRequest::new(&path, SaveKind::Save);
    request.overwrite_changed = true;
    document.save(request).wait().unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), b"a,b\nx,2\n");

    // Saved, the file is the one Leal knows: saving again needs no
    // agreement. Deleted, Save refuses; Save As still works.
    set(&document, 1, 1, "y");
    std::fs::remove_file(&path).unwrap();
    assert_eq!(
        save(&document, &path, SaveKind::Save).unwrap_err(),
        "Missing"
    );
    save(&document, &path, SaveKind::SaveAs).unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), b"a,b\nx,y\n");
}

/// A file replaced by another at its path (another app's safe save) is
/// changed elsewhere, however alike the two look.
#[test]
fn a_file_replaced_at_its_path_is_changed_elsewhere() {
    let dir = Dir::new("save-replaced");
    let scheduler = scheduler();
    let path = dir.file("a.csv", b"a,b\n");
    let document = open_at(&path, &dir, &scheduler);
    set(&document, 0, 0, "x");
    let other = dir.file("other.csv", b"a,b\n");
    std::fs::rename(&other, &path).unwrap();
    assert_eq!(
        save(&document, &path, SaveKind::Save).unwrap_err(),
        "ChangedElsewhere"
    );
}

// ---------------------------------------------------------------------------
// What a save keeps (DESIGN §3.7)

/// The file's permissions, its extended attributes and creation date are
/// kept; Leal's own attributes are rewritten or removed, never copied: an
/// unreadable one goes, and a remembered choice gets the new file's
/// fingerprint (ADR-0008 decision 8).
#[test]
fn a_save_keeps_the_files_metadata() {
    let dir = Dir::new("save-metadata");
    let scheduler = scheduler();
    let path = dir.file("a.csv", b"a;b\n1;2\n");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
    write_attribute(&path, c"com.example.note", Some(b"kept"));
    write_attribute(&path, INTERPRETATION_ATTRIBUTE_C, Some(b"v=2;unreadable"));
    let created = std::fs::metadata(&path).unwrap().created().unwrap();
    std::thread::sleep(Duration::from_millis(20));
    let document = open_at(&path, &dir, &scheduler);
    assert_eq!(document.detection().delimiter, Delimiter::Semicolon);
    set(&document, 1, 1, "3");
    let saved = save(&document, &path, SaveKind::Save).unwrap();
    let metadata = std::fs::metadata(&path).unwrap();
    assert_eq!(metadata.permissions().mode() & 0o7777, 0o640);
    assert_eq!(metadata.created().unwrap(), created);
    assert!(metadata.modified().unwrap() > created);
    assert_eq!(
        attribute(&path, c"com.example.note").as_deref(),
        Some(&b"kept"[..])
    );
    assert_eq!(
        saved.attributes.interpretation, None,
        "a reopen guesses the same"
    );
    assert_eq!(
        attribute(&path, INTERPRETATION_ATTRIBUTE_C),
        None,
        "the old one went"
    );
    assert_eq!(attribute(&path, TEXT_ENCODING_ATTRIBUTE_C), None);

    // A header choice remembered from an older version of the file, still
    // honoured (its delimiter fits): saved again with this file's
    // fingerprint.
    let stale = Interpretation {
        delimiter: Some(Delimiter::Semicolon),
        header: Some(false),
        file: Some(Fingerprint::of(b"elsewhere")),
    };
    let value = stale.to_attribute_value();
    write_attribute(&path, INTERPRETATION_ATTRIBUTE_C, Some(value.as_bytes()));
    let document = open_at(&path, &dir, &scheduler);
    assert_eq!(
        document.detection().header_source,
        crate::detect::DialectSource::Attribute
    );
    set(&document, 1, 1, "4");
    save(&document, &path, SaveKind::Save).unwrap();
    let fresh = Interpretation {
        file: Some(Fingerprint::of(&std::fs::read(&path).unwrap())),
        ..stale
    };
    assert_eq!(
        attribute(&path, INTERPRETATION_ATTRIBUTE_C),
        Some(fresh.to_attribute_value().into_bytes())
    );
}

/// The attributes record what a reopen would otherwise guess differently,
/// with the new file's fingerprint, and the user's choices.
#[test]
fn a_save_records_what_a_reopen_would_guess_differently() {
    let dir = Dir::new("save-attributes");
    let scheduler = scheduler();
    // Windows-1252 by its one high byte; an edit removes it, and what is
    // left (ASCII) would reopen as UTF-8.
    let path = dir.file("a.csv", b"id,name\n1,caf\xE9\n");
    let (document, _) = Document::open(
        &path,
        &dir.temp(),
        VolumeInfo::default(),
        &scheduler,
        OpenOptions {
            choices: Choices {
                header: Some(false),
                ..Choices::default()
            },
            ..options(30)
        },
        None,
    )
    .unwrap();
    wait_for_index(&document);
    let document = Arc::new(document);
    set(&document, 1, 1, "cafe");
    let saved = save(&document, &path, SaveKind::Save).unwrap();
    assert_eq!(saved.attributes.text_encoding, Some(Encoding::Windows1252));
    assert_eq!(
        attribute(&path, TEXT_ENCODING_ATTRIBUTE_C).as_deref(),
        Some(&b"windows-1252;1280"[..])
    );
    let out = std::fs::read(&path).unwrap();
    let expected = Interpretation {
        delimiter: Some(Delimiter::Comma),
        header: Some(false),
        file: Some(Fingerprint::of(&out)),
    };
    assert_eq!(saved.attributes.interpretation, Some(expected));
    assert_eq!(
        attribute(&path, INTERPRETATION_ATTRIBUTE_C),
        Some(expected.to_attribute_value().into_bytes())
    );
    // Reopened, it reads the same way.
    let reopened = open_at(&path, &dir, &scheduler);
    let detection = reopened.detection();
    assert_eq!(
        (detection.encoding, detection.delimiter, detection.header),
        (Encoding::Windows1252, Delimiter::Comma, false)
    );
    assert!(detection.notes.is_empty());
}

// ---------------------------------------------------------------------------
// The rebase (ADR-0008 decision 1)

/// Leal's own save is not an outside change: the watcher stays quiet, the
/// file reads unchanged and not diverged, and saving twice in a row never
/// shows it as changed. The document reads the saved file, with a new
/// generation, and no edits.
#[test]
fn saving_twice_never_shows_the_file_as_changed() {
    let dir = Dir::new("save-twice");
    let scheduler = scheduler();
    let path = dir.file("a.csv", b"a,b\n1,2\n");
    let document = open_at(&path, &dir, &scheduler);
    let reports = watch(&document);
    let generation = document.generation();
    set(&document, 1, 0, "x");
    let first = save(&document, &path, SaveKind::Save).unwrap();
    assert_eq!(first.original.state, OriginalState::Unchanged);
    assert!(!first.original.diverged);
    assert!(document.generation() > generation);
    assert_eq!(
        first.reread.as_ref().unwrap().generation,
        document.generation()
    );
    assert!(!document.has_edits());
    assert_eq!(
        text(&document.rows(0..2, 100).unwrap()),
        [["a", "b"], ["x", "2"]]
    );
    set(&document, 1, 1, "y");
    let second = save(&document, &path, SaveKind::Save).unwrap();
    assert!(!second.original.diverged);
    assert_eq!(std::fs::read(&path).unwrap(), b"a,b\nx,y\n");
    // The events from the two renames have long arrived by now.
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        reports
            .try_iter()
            .all(|s| s.state == OriginalState::Unchanged && !s.diverged),
        "the watcher reported a change"
    );
    assert_eq!(document.check_original().state, OriginalState::Unchanged);
    assert!(document.can_save());
    // A change made elsewhere after the save is still seen.
    std::fs::write(&path, b"a,b\nchanged,elsewhere\n").unwrap();
    wait_for_status(&reports, "the change", |s| {
        s.state == OriginalState::Changed
    });
}

/// The undo history carries on after a save (ADR-0008 decision 1): undo
/// and redo apply by value on the saved file. Undoing a hatched cell's edit
/// empties it (its delimiters are in the file now), and redo fills it again.
#[test]
fn undo_and_redo_carry_on_after_a_save() {
    let dir = Dir::new("save-undo");
    let scheduler = scheduler();
    let path = dir.file("a.csv", b"a,b\n1\n");
    let document = open_at(&path, &dir, &scheduler);
    let lineage = document.lineage();
    let edit = set(&document, 0, 1, "B");
    let hatched = set(&document, 1, 2, "z");
    save(&document, &path, SaveKind::Save).unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), b"a,B\n1,,z\n");
    assert_eq!(document.lineage(), lineage);

    document.apply(&hatched.inverse()).unwrap();
    document.apply(&edit.inverse()).unwrap();
    assert_eq!(document.full_value(1, 2).unwrap().as_deref(), Some(""));
    assert_eq!(document.full_value(0, 1).unwrap().as_deref(), Some("b"));
    save(&document, &path, SaveKind::Save).unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), b"a,b\n1,,\n");

    document.apply(&edit).unwrap();
    document.apply(&hatched).unwrap();
    save(&document, &path, SaveKind::Save).unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), b"a,B\n1,,z\n");
    // Undone again, back to the saved file: no edits.
    document.apply(&hatched.inverse()).unwrap();
    document.apply(&hatched).unwrap();
    assert!(!document.has_edits());
}

/// A save held at its third checkpoint (writing), and a function that
/// lets it go on.
fn held_save(document: &Arc<Document>, request: SaveRequest) -> (SaveJob, impl FnOnce()) {
    held_at(document, request, 2)
}

/// A save held at checkpoint `at` (or [`AT_SWAP`], just before the new file
/// goes into place), and a function that lets it go on.
fn held_at(document: &Arc<Document>, request: SaveRequest, at: usize) -> (SaveJob, impl FnOnce()) {
    let (reached, held) = (mpsc::channel(), mpsc::channel::<()>());
    let (tell, go) = (Mutex::new(reached.0), Mutex::new(held.1));
    let hook: crate::document::saving::ChunkHook = Arc::new(move |checkpoint| {
        if checkpoint == at {
            tell.lock().unwrap().send(()).unwrap();
            let _ = go.lock().unwrap().recv();
        }
    });
    let job = document.save_hooked(request, hook);
    reached.1.recv_timeout(LONG).unwrap();
    (job, move || held.0.send(()).unwrap())
}

use crate::document::saving::AT_SWAP;

/// `work` on another thread, which must finish within a second: the main
/// thread never waits for a save (DESIGN §3.9).
fn promptly<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> T {
    let (sent, got) = mpsc::channel();
    std::thread::spawn(move || sent.send(work()).unwrap());
    got.recv_timeout(Duration::from_secs(1))
        .expect("it waited for the save")
}

/// An edit, an undo and a read during a save neither wait for it nor are
/// lost: the file has the edits the save took, and the document carries the
/// later ones over as unsaved edits, including one setting a saved cell
/// back to its original value.
#[test]
fn an_edit_during_a_save_neither_waits_nor_is_lost() {
    let dir = Dir::new("save-edit-during");
    let bytes = sample(4 * SAVE_CHUNK_BYTES);
    let path = dir.file("a.csv", &bytes);
    let scheduler = scheduler();
    let document = open_at(&path, &dir, &scheduler);
    let before = set(&document, 1, 1, "before");
    set(&document, 3, 1, "kept");
    let version = document.edit_version();
    let (job, go_on) = held_save(&document, SaveRequest::new(&path, SaveKind::Save));
    assert_eq!(job.progress().snapshot_version, Some(version));
    let during = promptly({
        let document = Arc::clone(&document);
        let before = before.clone();
        move || {
            let command = document.set_cell(2, 1, "during").unwrap().unwrap();
            document.apply(&before.inverse()).unwrap();
            assert_eq!(
                document.full_value(2, 1).unwrap().as_deref(),
                Some("during")
            );
            command
        }
    });
    assert_eq!(document.edit_version(), version + 2);
    go_on();
    let saved = job.wait().unwrap().clone();
    assert_eq!(saved.edits_during_save, [(1, 1), (2, 1)]);
    let out = std::fs::read(&path).unwrap();
    let text = String::from_utf8_lossy(&out);
    assert!(text.contains(",before,") && text.contains(",kept,") && !text.contains("during"));
    // Unsaved now: the edit made during the save, and the undo.
    assert!(document.has_edits());
    assert_eq!(document.edited_cells(), 2);
    assert_eq!(
        document.full_value(2, 1).unwrap().as_deref(),
        Some("during")
    );
    assert_eq!(
        document.full_value(1, 1).unwrap(),
        Some(text_of(&bytes, 1, 1))
    );
    assert_eq!(document.full_value(3, 1).unwrap().as_deref(), Some("kept"));
    // The commands still apply, by value.
    document.apply(&during.inverse()).unwrap();
    document.apply(&before).unwrap();
    assert!(!document.has_edits(), "back to the saved file");
}

/// Cell `(row, column)`'s value in `sample`'s bytes.
fn text_of(bytes: &[u8], row: usize, column: usize) -> String {
    let line = bytes.split(|&b| b == b'\n').nth(row).unwrap();
    let field = line.split(|&b| b == b',').nth(column).unwrap();
    String::from_utf8(field.to_vec()).unwrap()
}

/// While a save runs, the file isn't read again ([`DocumentError::Saving`]);
/// afterwards it is.
#[test]
fn the_file_is_not_read_again_while_it_is_saved() {
    let dir = Dir::new("save-reinterpret");
    let path = dir.file("a.csv", &sample(4 * SAVE_CHUNK_BYTES));
    let scheduler = scheduler();
    let document = open_at(&path, &dir, &scheduler);
    set(&document, 1, 1, "x");
    let (job, go_on) = held_save(&document, SaveRequest::new(&path, SaveKind::Save));
    let header = Choices {
        header: Some(false),
        ..Choices::default()
    };
    let refused = promptly({
        let document = Arc::clone(&document);
        move || document.reinterpret(header, 10, 100).map(|_| ())
    });
    assert!(matches!(refused, Err(DocumentError::Saving)), "{refused:?}");
    go_on();
    job.wait().unwrap();
    document.reinterpret(header, 10, 100).unwrap();
}

/// A second save waits for the first, and can be cancelled meanwhile.
#[test]
fn a_save_queued_behind_another_can_be_cancelled() {
    let dir = Dir::new("save-queued");
    let path = dir.file("a.csv", &sample(4 * SAVE_CHUNK_BYTES));
    let scheduler = scheduler();
    let document = open_at(&path, &dir, &scheduler);
    set(&document, 1, 1, "x");
    let (first, go_on) = held_save(&document, SaveRequest::new(&path, SaveKind::Save));
    // Held at its first checkpoint, in its wait for its turn.
    let (second, let_go) = held_at(
        &document,
        SaveRequest::new(dir.0.join("b.csv"), SaveKind::SaveAs),
        0,
    );
    assert_eq!(second.progress().phase, crate::save::SavePhase::Queued);
    let_go();
    second.cancel();
    assert!(matches!(second.wait(), Err(SaveError::Cancelled)));
    assert!(!dir.0.join("b.csv").exists());
    go_on();
    first.wait().unwrap();
    assert_eq!(first.progress().phase, crate::save::SavePhase::Finished);
}

/// Every row reads at once after a save: the new reading's index is the
/// old one shifted, so a row far into the file can be read and edited
/// before the index pass has run again.
#[test]
fn every_row_reads_at_once_after_a_save() {
    let dir = Dir::new("save-rows-at-once");
    let bytes = sample(20 * 1024 * 1024);
    let path = dir.file("a.csv", &bytes);
    let gate = Arc::new(Gate::default());
    let scheduler = scheduler_with(Arc::clone(&gate));
    let document = open_at(&path, &dir, &scheduler);
    let rows = document.row_count();
    assert!(rows > 500_000, "{rows} rows");
    set(&document, 500_000, 1, "far, edited");
    set(&document, 10, 0, "near");
    // The new reading's index pass can't start: every row it reads comes
    // from the shifted index.
    gate.close();
    save(&document, &path, SaveKind::Save).unwrap();
    assert!(document.index_job().result().is_none(), "not indexed yet");
    assert_eq!(document.row_count(), rows);
    assert_eq!(
        document.full_value(500_000, 1).unwrap().as_deref(),
        Some("far, edited")
    );
    set(&document, 500_000, 2, "again");
    assert_eq!(document.row_count(), rows);
    gate.open();
    wait_for_index(&document);
    assert_eq!(document.row_count(), rows);
    assert_eq!(
        document.full_value(500_000, 2).unwrap().as_deref(),
        Some("again")
    );
    assert_eq!(document.full_value(10, 0).unwrap().as_deref(), Some("near"));
}

/// A file on a removable drive (here an APFS disk image) is saved on the
/// drive, and the document's new snapshot is read the removable way, never
/// mapped from the drive (ADR-0006): ordinary reads, then a copy on the
/// internal disk.
#[test]
fn a_file_saved_on_a_removable_drive_is_read_back_from_a_copy() {
    let image = DiskImage::new("APFS");
    let dir = Dir::new("save-removable");
    let bytes = sample(512 * 1024);
    let scheduler = scheduler();
    let (document, path) = open_on_image(&image, &dir, &bytes, &scheduler);
    let document = Arc::new(document);
    wait_for_index(&document);
    assert_eq!(document.storage(), Storage::Copy);
    set(&document, 4, 1, "edited");
    let saved = save(&document, &path, SaveKind::Save).unwrap();
    assert!(!saved.original.diverged);
    // The bytes written were teed to the internal disk: nothing is read
    // back from the drive.
    assert_eq!(document.storage(), Storage::Copy);
    assert!(document.source().can_vanish());
    wait_for_index(&document);
    assert_eq!(document.storage(), Storage::Copy);
    assert_eq!(
        document.full_value(4, 1).unwrap().as_deref(),
        Some("edited")
    );
    let out = std::fs::read(&path).unwrap();
    assert_eq!(out.len() as u64, saved.len);
    assert_eq!(
        leftovers(image.root(), &["a.csv", "NSIRD_Leal_test"]),
        [""; 0]
    );
    drop(document);
    drop(image);
}

// ---------------------------------------------------------------------------
// Save As from an incomplete document (ADR-0008 decision 6, ADR-0010)

/// Checks a Save As of an incomplete document: the copy is the file's
/// first rows, whole, with the edits, and Save is refused.
fn check_incomplete_save_as(document: &Arc<Document>, dir: &Dir, bytes: &[u8]) {
    let rows = document.row_count();
    assert!(rows > 3);
    assert!(!document.can_save());
    assert_eq!(
        save(document, &document.original().path, SaveKind::Save).unwrap_err(),
        "Incomplete"
    );
    set(document, 2, 1, "edited");
    let copy = dir.0.join("copy.csv");
    let saved = save(document, &copy, SaveKind::SaveAs).unwrap();
    assert!(!saved.complete);
    assert_eq!(saved.rows, rows);
    assert!(
        saved.estimated_rows > rows,
        "{} of {}",
        saved.rows,
        saved.estimated_rows
    );
    let out = std::fs::read(&copy).unwrap();
    // The file's first rows, cut at a row boundary, with the edit.
    let index = RowIndex::build(bytes, document.current().parser.dialect()).unwrap();
    let cut = index.rows_extent(0..rows).unwrap().end;
    let row2 = index.row_extent(2).unwrap();
    let field = {
        let parser = document.current().parser;
        parser.parse_row(&index, 2, bytes).unwrap().fields()[1].span()
    };
    assert!(field.end <= row2.end);
    assert_only_changed(
        &bytes[..cut],
        &out,
        &[Change::replace(field, b"edited".to_vec())],
    );
    assert_eq!(out.last(), Some(&b'\n'));
    // The document is the complete copy now, every row at once.
    assert_eq!(document.row_count(), rows);
    wait_for_index(document);
    assert_eq!(document.row_count(), rows);
    assert!(document.can_save());
    assert!(!document.has_edits());
}

#[test]
fn save_as_from_a_disconnected_document_writes_its_whole_rows() {
    let dir = Dir::new("save-disconnected");
    let bytes = sample(600 * 1024);
    let path = dir.file("usb.csv", &bytes);
    let source = Source::open_simulating_fault(
        &path,
        &dir.temp(),
        8192,
        Some(SimulatedFault::Disconnect {
            at: 300 * 1024 + 17,
        }),
    )
    .unwrap();
    let scheduler = scheduler();
    let (document, _) = Document::from_source(source, &scheduler, options(30), None).unwrap();
    assert_eq!(
        document.index_job().control().wait_timeout(LONG),
        Some(Err(JobError::Read(ReadErrorKind::Disconnected)))
    );
    check_incomplete_save_as(&Arc::new(document), &dir, &bytes);
}

#[test]
fn save_as_from_a_document_changed_while_read_writes_its_trusted_rows() {
    let dir = Dir::new("save-changed");
    let bytes = sample(600 * 1024);
    let path = dir.file("usb.csv", &bytes);
    let source = Source::open_simulating_fault(
        &path,
        &dir.temp(),
        8192,
        Some(SimulatedFault::Change { at: 200 * 1024 }),
    )
    .unwrap();
    let scheduler = scheduler();
    let (document, _) = Document::from_source(source, &scheduler, options(30), None).unwrap();
    assert_eq!(
        document.index_job().control().wait_timeout(LONG),
        Some(Err(JobError::Read(ReadErrorKind::ChangedOnDisk)))
    );
    check_incomplete_save_as(&Arc::new(document), &dir, &bytes);
}

#[test]
fn save_as_from_a_document_deleted_on_its_share_writes_its_whole_rows() {
    let dir = Dir::new("save-deleted");
    let bytes = sample(600 * 1024);
    let path = dir.file("share.csv", &bytes);
    let at = 300 * 1024;
    let share = SimulatedShare {
        hold_at: Some(at),
        failure: Some(SimulatedShareFailure {
            at,
            errno: libc::ESTALE,
            times: None,
            on_stat: false,
            partial: false,
        }),
        retry_delays: QUICK_RETRIES,
        ..SimulatedShare::default()
    };
    let source = Source::open_simulating_share(&path, &dir.temp(), 8192, share).unwrap();
    let scheduler = scheduler();
    let (document, _) = Document::from_source(source, &scheduler, options(30), None).unwrap();
    std::fs::remove_file(&path).unwrap();
    document.source().simulated_share_release();
    assert_eq!(
        document.index_job().control().wait_timeout(LONG),
        Some(Err(JobError::Read(ReadErrorKind::Deleted)))
    );
    check_incomplete_save_as(&Arc::new(document), &dir, &bytes);
}

/// The exFAT disk-image case: a drive that can't clone, so Leal reads the
/// user's file itself, pulled before the copy began. Leal trusts the rows
/// of the first 64 KB it read at first paint, and Save As writes those,
/// whole.
#[test]
fn save_as_from_an_exfat_drive_pulled_before_the_copy_writes_its_whole_rows() {
    let image = DiskImage::new("ExFAT");
    let dir = Dir::new("save-exfat");
    let bytes = sample(2 * 1024 * 1024);
    let gate = Gate::closed();
    let scheduler = scheduler_with(Arc::clone(&gate));
    let (document, _) = open_on_image(&image, &dir, &bytes, &scheduler);
    let document = Arc::new(document);
    image.force_detach();
    gate.open();
    assert_eq!(
        document.index_job().control().wait_timeout(LONG),
        Some(Err(JobError::Read(ReadErrorKind::Disconnected)))
    );
    assert_eq!(document.storage(), Storage::Disconnected);
    check_incomplete_save_as(&document, &dir, &bytes);
    drop(document);
    drop(image);
}

// ---------------------------------------------------------------------------
// NSDocument's guards, now the core's (ADR-0012 decision 1)

/// Runs `tool` (a full path) with `args` and checks it worked.
fn run(tool: &str, args: &[&str]) {
    let status = std::process::Command::new(tool)
        .args(args)
        .status()
        .unwrap();
    assert!(status.success(), "{tool} {args:?}");
}

fn path_str(path: &Path) -> &str {
    path.to_str().unwrap()
}

/// A file Leal may not write, or a locked one, is refused before anything
/// is written, each with its own reason: a rename would replace it anyway.
#[test]
fn unwritable_and_locked_files_are_refused_before_writing() {
    let dir = Dir::new("save-guards");
    let scheduler = scheduler();
    let path = dir.file("read-only.csv", b"a,b\n");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o444)).unwrap();
    let document = open_at(&path, &dir, &scheduler);
    set(&document, 0, 0, "x");
    assert_eq!(
        save(&document, &path, SaveKind::Save).unwrap_err(),
        "NotWritable"
    );
    assert_eq!(std::fs::read(&path).unwrap(), b"a,b\n");

    let locked = dir.file("locked.csv", b"a,b\n");
    let document = open_at(&locked, &dir, &scheduler);
    set(&document, 0, 0, "x");
    run("/usr/bin/chflags", &["uchg", path_str(&locked)]);
    let refused = save(&document, &locked, SaveKind::Save).unwrap_err();
    run("/usr/bin/chflags", &["nouchg", path_str(&locked)]);
    assert_eq!(refused, "Locked");
    assert_eq!(std::fs::read(&locked).unwrap(), b"a,b\n");
    assert_eq!(
        leftovers(
            &dir.0,
            &["read-only.csv", "locked.csv", "scratch", "records"]
        ),
        [""; 0]
    );
}

/// A file someone else owns (the system's `/etc/hosts`, root's and mode
/// 0644): Leal can read it but not write it, so Save refuses before it
/// makes anything in `/etc`.
#[test]
fn a_file_owned_by_someone_else_is_not_writable() {
    let hosts = Path::new("/private/etc/hosts");
    let metadata = std::fs::metadata(hosts).unwrap();
    if std::os::unix::fs::MetadataExt::uid(&metadata) == 0
        && crate::source::can_write(hosts).unwrap()
    {
        return; // running as root: nothing to test
    }
    let dir = Dir::new("save-hosts");
    let scheduler = scheduler();
    let document = open_at(hosts, &dir, &scheduler);
    set(&document, 0, 0, "x");
    assert_eq!(
        save(&document, hosts, SaveKind::Save).unwrap_err(),
        "NotWritable"
    );
}

/// The metadata policy (ADR-0012 decision 1): tags and Finder's info are
/// kept, quarantine isn't (the system's rule for a safe save), the
/// system's own attributes (`provenance` here) are never copied, and an
/// access control list that denies writing attributes, set last, stops
/// nothing. (An attribute that can't be set is skipped and named:
/// `source::tests::an_attribute_that_cant_be_set_is_skipped_and_named`.)
#[test]
fn the_metadata_policy_keeps_what_belongs_and_skips_what_cant_be_set() {
    let dir = Dir::new("save-policy");
    let scheduler = scheduler();
    let path = dir.file("a.csv", b"a,b\n");
    let tags = b"bplist00\xa1\x01UGreen\n\x08\x0a";
    write_attribute(&path, c"com.apple.metadata:_kMDItemUserTags", Some(tags));
    let finder = [0_u8; 32];
    let mut finder = finder;
    finder[8] = 0x40; // the "has custom icon" bit's byte, any content
    write_attribute(&path, c"com.apple.FinderInfo", Some(&finder));
    write_attribute(&path, c"com.apple.quarantine", Some(b"0081;5f000000;Leal;"));
    // A protected attribute, if this process may make one at all.
    let protected = crate::source::tests::try_write_attribute(
        &path,
        c"com.apple.provenance",
        b"\x01\x02\x00\x00\x00\x00\x00\x00\x00\x00\x00",
    );
    run(
        "/bin/chmod",
        &["+a", "everyone deny writeextattr", path_str(&path)],
    );
    let document = open_at(&path, &dir, &scheduler);
    set(&document, 0, 0, "x");
    let saved = save(&document, &path, SaveKind::Save).unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), b"x,b\n");
    assert_eq!(
        attribute(&path, c"com.apple.metadata:_kMDItemUserTags").as_deref(),
        Some(&tags[..])
    );
    assert_eq!(
        attribute(&path, c"com.apple.FinderInfo").as_deref(),
        Some(&finder[..])
    );
    let listed = std::process::Command::new("/usr/bin/xattr")
        .args(["-l", path_str(&path)])
        .output()
        .unwrap();
    assert_eq!(
        attribute(&path, c"com.apple.quarantine"),
        None,
        "{}",
        String::from_utf8_lossy(&listed.stdout)
    );
    let acl = std::process::Command::new("/bin/ls")
        .args(["-le", path_str(&path)])
        .output()
        .unwrap();
    let acl = String::from_utf8_lossy(&acl.stdout);
    assert!(acl.contains("deny writeextattr"), "{acl}");
    if protected {
        assert_ne!(
            attribute(&path, c"com.apple.provenance").as_deref(),
            Some(&b"\x01\x02\x00\x00\x00\x00\x00\x00\x00\x00\x00"[..]),
            "the system's own, never copied"
        );
    }
    assert!(
        saved.skipped_metadata.is_empty(),
        "{:?}",
        saved.skipped_metadata
    );
    run("/bin/chmod", &["-N", path_str(&path)]);
}

/// The flags a user sets are kept (hidden, here); the compression flag,
/// which describes the old file's blocks, isn't, nor its compression
/// header: the new file's bytes are plain. (`ditto --hfsCompression`
/// compresses nothing on some Macs, this one among them in October 2026;
/// there, only the hidden flag is checked.)
#[test]
fn a_save_keeps_the_hidden_flag_but_not_compression() {
    use std::os::macos::fs::MetadataExt as _;
    let dir = Dir::new("save-flags");
    let scheduler = scheduler();
    let plain = dir.file("plain.csv", &sample(64 * 1024));
    let path = dir.0.join("a.csv");
    run(
        "/usr/bin/ditto",
        &["--hfsCompression", path_str(&plain), path_str(&path)],
    );
    run("/usr/bin/chflags", &["hidden", path_str(&path)]);
    let flags = std::fs::metadata(&path).unwrap().st_flags();
    let compressed = flags & libc::UF_COMPRESSED != 0;
    if !compressed {
        eprintln!("ditto didn't compress the file: only the hidden flag is checked");
    }
    assert_ne!(flags & libc::UF_HIDDEN, 0);
    let document = open_at(&path, &dir, &scheduler);
    set(&document, 1, 1, "edited");
    save(&document, &path, SaveKind::Save).unwrap();
    let flags = std::fs::metadata(&path).unwrap().st_flags();
    assert_ne!(flags & libc::UF_HIDDEN, 0, "kept hidden");
    assert_eq!(flags & libc::UF_COMPRESSED, 0, "not compressed");
    assert_eq!(attribute(&path, c"com.apple.decmpfs"), None);
    let out = std::fs::read(&path).unwrap();
    assert!(String::from_utf8_lossy(&out).contains(",edited,"));
}

/// A symbolic link to the file stays a link: the file it leads to is saved.
#[test]
fn a_symlinked_destination_stays_a_link() {
    let dir = Dir::new("save-symlink");
    let scheduler = scheduler();
    let target = dir.file("target.csv", b"a,b\n");
    let link = dir.0.join("link.csv");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    let document = open_at(&link, &dir, &scheduler);
    set(&document, 0, 1, "y");
    let saved = save(&document, &link, SaveKind::Save).unwrap();
    assert!(
        std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(std::fs::read(&target).unwrap(), b"a,y\n");
    assert_eq!(saved.path, target.canonicalize().unwrap());
}

/// A change to the file that the watcher sees while the save writes is
/// refused at the swap, and the other app's bytes are kept: even one that
/// keeps its length and puts its modification time back, which only the
/// watcher's event shows.
#[test]
fn a_change_seen_during_a_save_is_refused() {
    let dir = Dir::new("save-changed-during");
    let bytes = sample(4 * SAVE_CHUNK_BYTES);
    let path = dir.file("a.csv", &bytes);
    let scheduler = scheduler();
    let document = open_at(&path, &dir, &scheduler);
    let reports = watch(&document);
    set(&document, 1, 1, "x");
    let (job, go_on) = held_save(&document, SaveRequest::new(&path, SaveKind::Save));
    let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
    let file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
    std::os::unix::fs::FileExt::write_all_at(&file, b"elsewhere", 100).unwrap();
    file.set_modified(modified).unwrap();
    drop(file);
    assert_eq!(std::fs::metadata(&path).unwrap().len(), bytes.len() as u64);
    assert_eq!(
        std::fs::metadata(&path).unwrap().modified().unwrap(),
        modified
    );
    wait_for_status(&reports, "the change", |s| {
        s.state == OriginalState::Changed
    });
    go_on();
    assert!(matches!(job.wait(), Err(SaveError::ChangedElsewhere)));
    assert_eq!(&std::fs::read(&path).unwrap()[100..109], b"elsewhere");
    assert!(document.has_edits());
}

/// A Save inside the window after the file was renamed, while it may be
/// another app's save's backup step, is refused as moving, not missing.
#[test]
fn a_save_while_the_file_is_moving_is_refused_as_moving() {
    let dir = Dir::new("save-moving");
    let scheduler = scheduler();
    let path = dir.file("a.csv", b"a,b\n");
    let document = open_at(&path, &dir, &scheduler);
    let reports = watch(&document);
    set(&document, 0, 0, "x");
    std::fs::rename(&path, dir.0.join("b.csv")).unwrap();
    // The watcher sees the rename and holds it as pending.
    let deadline = std::time::Instant::now() + LONG;
    while !document.original.is_moving() {
        assert!(
            std::time::Instant::now() < deadline,
            "the rename wasn't seen"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    let _ = reports.try_iter().count();
    assert_eq!(
        save(&document, &path, SaveKind::Save).unwrap_err(),
        "Moving"
    );
}

/// Save over a file on an exFAT drive (a disk image): it can't clone, so
/// the bytes are teed to the internal disk, and the file is replaced (or,
/// where exFAT can't swap, renamed over).
#[test]
fn a_file_on_exfat_is_saved_over() {
    let image = DiskImage::new("ExFAT");
    let dir = Dir::new("save-exfat-over");
    let bytes = sample(300 * 1024);
    let scheduler = scheduler();
    let (document, path) = open_on_image(&image, &dir, &bytes, &scheduler);
    let document = Arc::new(document);
    wait_for_index(&document);
    set(&document, 3, 1, "edited");
    let saved = save(&document, &path, SaveKind::Save).unwrap();
    let out = std::fs::read(&path).unwrap();
    assert_eq!(out.len() as u64, saved.len);
    assert!(String::from_utf8_lossy(&out).contains(",edited,"));
    assert_eq!(document.storage(), Storage::Copy);
    // exFAT can't swap.
    assert_eq!(saved.placed, Placed::Renamed);
    // Save again: the document's identity of the file still holds.
    set(&document, 4, 1, "again");
    save(&document, &path, SaveKind::Save).unwrap();
    drop(document);
    drop(image);
}

/// A CRLF file's incomplete Save As is cut after a whole CRLF.
#[test]
fn an_incomplete_crlf_file_is_cut_after_a_whole_line_ending() {
    let dir = Dir::new("save-crlf-cut");
    let bytes: Vec<u8> = sample(600 * 1024)
        .split(|&b| b == b'\n')
        .flat_map(|line| [line, b"\r\n"].concat())
        .collect();
    let path = dir.file("usb.csv", &bytes);
    let source = Source::open_simulating_fault(
        &path,
        &dir.temp(),
        8192,
        Some(SimulatedFault::Disconnect { at: 300 * 1024 + 1 }),
    )
    .unwrap();
    let scheduler = scheduler();
    let (document, _) = Document::from_source(source, &scheduler, options(30), None).unwrap();
    let _ = document.index_job().control().wait_timeout(LONG);
    let document = Arc::new(document);
    let copy = dir.0.join("copy.csv");
    let saved = save(&document, &copy, SaveKind::SaveAs).unwrap();
    assert!(!saved.complete);
    let out = std::fs::read(&copy).unwrap();
    assert!(out.ends_with(b"\r\n"));
    assert_eq!(&bytes[..out.len()], &out[..]);
}

/// Save over a file on a FAT32 drive (a disk image), which can't swap (its
/// driver says a swap worked and renames instead): renamed over, and saved
/// again.
#[test]
fn a_file_on_fat32_is_renamed_over() {
    let image = DiskImage::new("MS-DOS FAT32");
    let dir = Dir::new("save-fat32");
    let bytes = sample(300 * 1024);
    let scheduler = scheduler();
    let (document, path) = open_on_image(&image, &dir, &bytes, &scheduler);
    let document = Arc::new(document);
    wait_for_index(&document);
    set(&document, 3, 1, "edited");
    let saved = save(&document, &path, SaveKind::Save).unwrap();
    assert_eq!(saved.placed, Placed::Renamed);
    assert_eq!(saved.kept, None);
    let out = std::fs::read(&path).unwrap();
    assert_eq!(out.len() as u64, saved.len);
    assert!(String::from_utf8_lossy(&out).contains(",edited,"));
    assert!(!saved.original.diverged);
    set(&document, 4, 1, "again");
    save(&document, &path, SaveKind::Save).unwrap();
    assert!(String::from_utf8_lossy(&std::fs::read(&path).unwrap()).contains(",again,"));
    assert_eq!(
        leftovers(image.root(), &["a.csv", "NSIRD_Leal_test"]),
        [""; 0]
    );
    drop(document);
    drop(image);
}

/// Save As to a new name on an exFAT drive, which can't rename exclusively:
/// nothing there, so a plain rename puts it there.
#[test]
fn save_as_to_a_new_name_on_exfat() {
    let image = DiskImage::new("ExFAT");
    let dir = Dir::new("save-as-exfat");
    let bytes = sample(200 * 1024);
    let path = dir.file("a.csv", &bytes);
    let scheduler = scheduler();
    let document = open_at(&path, &dir, &scheduler);
    set(&document, 2, 1, "edited");
    let copy = image.root().join("copy.csv");
    let saved = save(&document, &copy, SaveKind::SaveAs).unwrap();
    assert_eq!(saved.placed, Placed::Renamed);
    let out = std::fs::read(&copy).unwrap();
    assert_eq!(out.len() as u64, saved.len);
    assert!(String::from_utf8_lossy(&out).contains(",edited,"));
    assert_eq!(
        std::fs::read(&path).unwrap(),
        bytes,
        "the original is untouched"
    );
    assert_eq!(leftovers(image.root(), &["copy.csv"]), [""; 0]);
    drop(document);
    drop(image);
}

/// The app's path: the empty folder it makes on the destination's volume
/// (`NSFileManager`'s item-replacement folder) holds the new file, and is
/// gone afterwards.
#[test]
fn the_apps_replacement_folder_holds_the_new_file_and_goes() {
    let dir = Dir::new("save-replacement-folder");
    let scheduler = scheduler();
    let path = dir.file("a.csv", b"a,b\n1,2\n");
    let document = open_at(&path, &dir, &scheduler);
    set(&document, 1, 1, "x");
    let folder = dir.0.join("(A Document Being Saved By Leal)");
    std::fs::create_dir(&folder).unwrap();
    let mut request = SaveRequest::new(&path, SaveKind::Save);
    request.folder = Some(folder.clone());
    let job = document.save(request);
    job.wait().unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), b"a,b\n1,x\n");
    assert!(!folder.exists());
    assert_eq!(leftovers(&dir.0, &["a.csv", "scratch", "records"]), [""; 0]);
}

/// A replacement folder on another volume (here a disk image) can't hold
/// the new file, which a rename couldn't move: it is removed, and the new
/// file is made in a hidden folder beside the destination instead.
#[test]
fn a_replacement_folder_on_another_volume_is_not_used() {
    let image = DiskImage::new("APFS");
    let dir = Dir::new("save-replacement-elsewhere");
    let scheduler = scheduler();
    let path = dir.file("a.csv", b"a,b\n1,2\n");
    let document = open_at(&path, &dir, &scheduler);
    set(&document, 1, 1, "x");
    let folder = image.root().join("replacement");
    std::fs::create_dir(&folder).unwrap();
    let mut request = SaveRequest::new(&path, SaveKind::Save);
    request.folder = Some(folder.clone());
    let job = document.save(request);
    job.wait().unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), b"a,b\n1,x\n");
    assert!(!folder.exists());
    assert_eq!(leftovers(&dir.0, &["a.csv", "scratch", "records"]), [""; 0]);
    drop(document);
    drop(image);
}

/// While the new file goes into place (the save held there, under the
/// watcher's lock), the writer lock is free: an edit and an undo return at
/// once, and are carried over.
#[test]
fn edits_while_the_file_goes_into_place_dont_wait() {
    let dir = Dir::new("save-edit-at-swap");
    let bytes = sample(64 * 1024);
    let path = dir.file("a.csv", &bytes);
    let scheduler = scheduler();
    let document = open_at(&path, &dir, &scheduler);
    let before = set(&document, 1, 1, "before");
    let (job, go_on) = held_at(&document, SaveRequest::new(&path, SaveKind::Save), AT_SWAP);
    assert_eq!(job.progress().phase, crate::save::SavePhase::Replacing);
    promptly({
        let document = Arc::clone(&document);
        move || {
            document.set_cell(2, 1, "during").unwrap().unwrap();
            document.apply(&before.inverse()).unwrap();
        }
    });
    go_on();
    let saved = job.wait().unwrap().clone();
    assert_eq!(saved.edits_during_save, [(1, 1), (2, 1)]);
    let text = String::from_utf8(std::fs::read(&path).unwrap()).unwrap();
    assert!(text.contains(",before,") && !text.contains("during"));
    assert_eq!(
        document.full_value(2, 1).unwrap().as_deref(),
        Some("during")
    );
    assert_eq!(
        document.full_value(1, 1).unwrap(),
        Some(text_of(&bytes, 1, 1))
    );
}

/// The file's metadata changed while the save ran, its contents not: a tag
/// added then is on the new file (its metadata is copied again), and a
/// file made read-only then is refused, untouched.
#[test]
fn metadata_changed_during_a_save_is_copied_again_or_refused() {
    let dir = Dir::new("save-metadata-during");
    let scheduler = scheduler();
    let bytes = sample(64 * 1024);
    let path = dir.file("a.csv", &bytes);
    let document = open_at(&path, &dir, &scheduler);
    set(&document, 1, 1, "x");
    let (job, go_on) = held_at(&document, SaveRequest::new(&path, SaveKind::Save), AT_SWAP);
    let tags = b"bplist00\xa1\x01UGreen\n\x08\x0a";
    write_attribute(&path, c"com.apple.metadata:_kMDItemUserTags", Some(tags));
    go_on();
    job.wait().unwrap();
    assert_eq!(
        attribute(&path, c"com.apple.metadata:_kMDItemUserTags").as_deref(),
        Some(&tags[..])
    );
    assert!(String::from_utf8_lossy(&std::fs::read(&path).unwrap()).contains(",x,"));

    set(&document, 2, 1, "y");
    let before = std::fs::read(&path).unwrap();
    let (job, go_on) = held_at(&document, SaveRequest::new(&path, SaveKind::Save), AT_SWAP);
    run("/bin/chmod", &["a-w", path_str(&path)]);
    go_on();
    let refused = job.wait().map(|_| ());
    run("/bin/chmod", &["u+w", path_str(&path)]);
    assert!(
        matches!(refused, Err(SaveError::NotWritable)),
        "{refused:?}"
    );
    assert_eq!(std::fs::read(&path).unwrap(), before);
    assert!(document.has_edits());
    assert_eq!(leftovers(&dir.0, &["a.csv", "scratch", "records"]), [""; 0]);
}

/// A check of the file during a save that finds its drive back doesn't
/// reconnect then (the save may replace the reading); the save does, once
/// it ends, and says so.
#[test]
fn a_drive_back_during_a_save_is_reconnected_when_it_ends() {
    let dir = Dir::new("save-drive-back");
    let bytes = sample(600 * 1024);
    let path = dir.file("usb.csv", &bytes);
    let source = Source::open_simulating_fault(
        &path,
        &dir.temp(),
        8192,
        Some(SimulatedFault::Disconnect { at: 300 * 1024 }),
    )
    .unwrap();
    let scheduler = scheduler();
    let (document, _) = Document::from_source(source, &scheduler, options(30), None).unwrap();
    let _ = document.index_job().control().wait_timeout(LONG);
    let document = Arc::new(document);
    assert_eq!(document.storage(), Storage::Disconnected);
    let generation = document.generation();
    let copy = dir.0.join("copy.csv");
    let (job, go_on) = held_at(&document, SaveRequest::new(&copy, SaveKind::SaveAs), 0);
    document.source().simulate_drive_back();
    let (status, restarted) = document.check_original_restarting();
    assert_eq!(status.state, OriginalState::Unchanged);
    assert_eq!(restarted, None, "not while the save runs");
    assert_eq!(document.storage(), Storage::Disconnected);
    assert_eq!(document.generation(), generation);
    job.cancel();
    go_on();
    assert!(matches!(job.wait(), Err(SaveError::Cancelled)));
    assert_eq!(
        job.restarted(),
        Some(Restarted {
            from: generation,
            to: document.generation(),
        })
    );
    assert_ne!(document.generation(), generation);
    assert_ne!(document.storage(), Storage::Disconnected);
    wait_for_index(&document);
    assert!(document.can_save());
}

/// A save whose file can't be read back (here its snapshot is deleted
/// before the rebase) has still saved; the document keeps reading the old
/// snapshot, and reports the cells edited since the save's snapshot.
#[test]
fn a_save_that_cant_read_its_file_back_reports_the_edits_since_its_snapshot() {
    let dir = Dir::new("save-no-reread");
    let bytes = sample(64 * 1024);
    let path = dir.file("a.csv", &bytes);
    let scheduler = scheduler();
    let document = open_at(&path, &dir, &scheduler);
    set(&document, 1, 1, "saved");
    let scratch = dir.0.join("scratch");
    let before = leftovers(&scratch, &[]);
    let (job, go_on) = held_at(&document, SaveRequest::new(&path, SaveKind::Save), AT_SWAP);
    for folder in leftovers(&scratch, &[]) {
        if !before.contains(&folder) {
            for file in std::fs::read_dir(scratch.join(&folder)).unwrap() {
                std::fs::remove_file(file.unwrap().path()).unwrap();
            }
        }
    }
    document.set_cell(2, 1, "during").unwrap().unwrap();
    go_on();
    let saved = job.wait().unwrap().clone();
    assert!(saved.reread.is_none());
    assert!(saved.reread_error.is_some());
    assert_eq!(saved.edits_during_save, [(2, 1)]);
    assert!(String::from_utf8_lossy(&std::fs::read(&path).unwrap()).contains(",saved,"));
    assert_eq!(
        document.full_value(2, 1).unwrap().as_deref(),
        Some("during")
    );
    assert!(document.has_edits());
}

/// Save As onto something other than a regular file (a folder) is refused,
/// and it is left as it was.
#[test]
fn save_as_onto_a_folder_is_refused() {
    let dir = Dir::new("save-as-folder");
    let scheduler = scheduler();
    let path = dir.file("a.csv", b"a,b\n");
    let document = open_at(&path, &dir, &scheduler);
    let folder = dir.0.join("folder.csv");
    std::fs::create_dir(&folder).unwrap();
    assert_eq!(
        save(&document, &folder, SaveKind::SaveAs).unwrap_err(),
        "NotAFile"
    );
    assert!(folder.is_dir());
    assert_eq!(
        leftovers(&dir.0, &["a.csv", "folder.csv", "scratch", "records"]),
        [""; 0]
    );
}
