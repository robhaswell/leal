//! What Swift sees of saving: the job, its outcome and its refusals, and
//! the document reading the saved file afterwards.

use super::*;

use crate::document::OriginalState;
use crate::document::tests::{TempDir, block_on, options};
use crate::{Scheduler, VolumeInfo, open_document};

fn open(dir: &TempDir, scheduler: &Scheduler, name: &str, bytes: &[u8]) -> (Arc<Document>, String) {
    let path = dir.file(name, bytes);
    let document = open_document(
        &path,
        VolumeInfo::default(),
        dir.locations(),
        scheduler,
        options(),
        None,
    )
    .unwrap();
    assert_eq!(block_on(document.index_job().unwrap().wait()), Ok(()));
    (document, path)
}

fn request(destination: &str, kind: SaveKind) -> SaveOptions {
    SaveOptions {
        destination: destination.to_owned(),
        kind,
        folder: None,
        volume: VolumeInfo::default(),
        overwrite_changed: false,
        first_screen_rows: 10,
        max_chars: 100,
    }
}

#[test]
fn a_save_writes_the_edit_and_the_document_reads_the_saved_file() {
    let dir = TempDir::new("save");
    let scheduler = Scheduler::new().unwrap();
    let (document, path) = open(&dir, &scheduler, "a.csv", b"id,name\n1,Ada\n");
    let generation = document.first_screen().unwrap().generation;
    let command = document.set_cell(1, 1, "Ada, Countess").unwrap().unwrap();
    let job = Arc::clone(&document)
        .save(request(&path, SaveKind::Save))
        .unwrap();
    let outcome = block_on(job.wait()).unwrap();
    assert_eq!(
        std::fs::read(&path).unwrap(),
        b"id,name\n1,\"Ada, Countess\"\n"
    );
    assert_eq!(outcome.byte_count, 26);
    assert_eq!((outcome.row_count, outcome.complete), (2, true));
    assert_eq!(outcome.original.state, OriginalState::Unchanged);
    assert!(!outcome.original.diverged);
    let screen = outcome.first_screen.unwrap();
    assert!(screen.generation > generation);
    assert_eq!(document.first_screen().unwrap(), screen);
    assert!(!document.has_unsaved_edits().unwrap());
    assert_eq!(
        job.progress(),
        SaveProgress {
            phase: SavePhase::Finished,
            written: 26,
            total: 26,
            snapshot_version: Some(1),
        }
    );
    assert_eq!(
        outcome.modified,
        std::fs::metadata(&path).unwrap().modified().ok()
    );
    assert!(outcome.swapped);
    assert!(outcome.edits_during_save.is_empty() && outcome.skipped_metadata.is_empty());
    // The undo history carries on.
    document.undo(command).unwrap();
    assert!(document.has_unsaved_edits().unwrap());
    let again = Arc::clone(&document)
        .save(request(&path, SaveKind::Save))
        .unwrap();
    block_on(again.wait()).unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), b"id,name\n1,\"Ada\"\n");
}

#[test]
fn refusals_reach_swift_with_their_reasons() {
    let dir = TempDir::new("save-refused");
    let scheduler = Scheduler::new().unwrap();
    let (document, path) = open(&dir, &scheduler, "a.csv", b"a,b\n");
    document.set_cell(0, 0, "x").unwrap();
    std::fs::write(&path, b"a,b,c\n").unwrap();
    let job = Arc::clone(&document)
        .save(request(&path, SaveKind::Save))
        .unwrap();
    assert_eq!(block_on(job.wait()), Err(SaveFailure::ChangedElsewhere));
    std::fs::remove_file(&path).unwrap();
    let job = Arc::clone(&document)
        .save(request(&path, SaveKind::Save))
        .unwrap();
    assert_eq!(block_on(job.wait()), Err(SaveFailure::Missing));

    let (single, path) = open(&dir, &scheduler, "w.csv", b"caf\xE9\n");
    single.set_cell(0, 0, "\u{e9}").unwrap();
    let job = Arc::clone(&single)
        .save(request(&path, SaveKind::SaveAs))
        .unwrap();
    assert_eq!(
        block_on(job.wait()),
        Err(SaveFailure::EncodingNotSupported {
            encoding: TextEncoding::Windows1252,
            cells: vec![CellPlace { row: 0, column: 0 }],
        })
    );
}

/// A restart relabels the cached first screen only if it is the screen of
/// the reading restarted: once a save has replaced that reading, the cache
/// keeps the save's screen and generation (a check racing a save's end
/// relabelled the old screen).
#[test]
fn a_restart_relabels_only_its_own_reading() {
    let dir = TempDir::new("save-restart-label");
    let scheduler = Scheduler::new().unwrap();
    let (document, path) = open(&dir, &scheduler, "a.csv", b"id,name\n1,Ada\n");
    document.set_cell(1, 1, "Grace").unwrap().unwrap();
    let before = document.first_screen().unwrap().generation;
    let job = Arc::clone(&document)
        .save(request(&path, SaveKind::Save))
        .unwrap();
    let saved = block_on(job.wait()).unwrap().first_screen.unwrap();
    assert!(saved.generation > before);
    // A restart of the reading before the save, reported late.
    document.adopt_restart(leal_core::document::Restarted {
        from: before,
        to: saved.generation + 1,
    });
    assert_eq!(document.first_screen().unwrap(), saved);
    // One of the current reading.
    document.adopt_restart(leal_core::document::Restarted {
        from: saved.generation,
        to: saved.generation + 1,
    });
    assert_eq!(
        document.first_screen().unwrap().generation,
        saved.generation + 1
    );
}
