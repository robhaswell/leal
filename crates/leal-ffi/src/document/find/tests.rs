//! What Swift sees of find, copy and the inspector's values.

use super::*;

use crate::document::tests::{TempDir, block_on, options};
use crate::{Scheduler, VolumeInfo, open_document};

fn orders(dir: &TempDir, scheduler: &Scheduler) -> Arc<Document> {
    let path = dir.file(
        "orders.csv",
        "id,customer,notes\n\
         1,Marlow Foods,\"say \"\"hi\"\"\tthere\"\n\
         2,Ostrava,\"two\nlines marlow\"\n\
         3,caf\u{e9},x\n"
            .as_bytes(),
    );
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
    document
}

#[test]
fn a_search_reaches_swift() {
    let dir = TempDir::new("find");
    let scheduler = Scheduler::new().unwrap();
    let document = orders(&dir, &scheduler);
    assert!(document.find(String::new(), false).unwrap().is_none());
    let search = document.find("MARLOW".to_owned(), false).unwrap().unwrap();
    assert_eq!(block_on(search.job().wait()), Ok(()));
    assert_eq!(
        search.progress().unwrap(),
        SearchProgress {
            generation: 0,
            matches: 2,
            rows_searched: 4,
            complete: true,
            catching_up: false,
        }
    );
    assert_eq!(
        search.step(None, 0, true).unwrap(),
        SearchStep::Found {
            row: 1,
            column: 1,
            ordinal: 1,
            wrapped: false
        }
    );
    assert_eq!(
        search.step(Some(1), 1, true).unwrap(),
        SearchStep::Found {
            row: 2,
            column: 2,
            ordinal: 2,
            wrapped: false
        }
    );
    assert_eq!(
        search.step(Some(2), 2, true).unwrap(),
        SearchStep::Found {
            row: 1,
            column: 1,
            ordinal: 1,
            wrapped: true
        }
    );
    assert_eq!(search.ordinal(2, 2).unwrap(), Some(2));
    assert_eq!(search.ordinal(3, 1).unwrap(), None);
    assert_eq!(
        search.matches_in(0, 64, 0, 32, 128).unwrap(),
        [
            CellMatch {
                row: 1,
                column: 1,
                ranges: vec![TextRange {
                    start: 0,
                    length: 6
                }],
            },
            CellMatch {
                row: 2,
                column: 2,
                ranges: vec![TextRange {
                    start: 10,
                    length: 6
                }],
            },
        ]
    );
    let exact = document.find("MARLOW".to_owned(), true).unwrap().unwrap();
    assert_eq!(block_on(exact.job().wait()), Ok(()));
    assert_eq!(exact.step(None, 0, false).unwrap(), SearchStep::NotFound);
    // Cancelling a finished search changes nothing.
    search.cancel();
    assert_eq!(search.progress().unwrap().matches, 2);
}

#[test]
fn a_copy_and_a_cell_value_reach_swift() {
    let dir = TempDir::new("copy");
    let scheduler = Scheduler::new().unwrap();
    let document = orders(&dir, &scheduler);
    let copy = document.copy_cells(1, 2, 1, 2).unwrap();
    assert_eq!(copy.take().unwrap(), None, "not finished yet, or taken");
    assert_eq!(block_on(copy.job().wait()), Ok(()));
    assert_eq!(
        copy.take().unwrap().as_deref(),
        Some("Marlow Foods\t\"say \"\"hi\"\"\tthere\"\nOstrava\t\"two\nlines marlow\"")
    );
    assert_eq!(copy.take().unwrap(), None);
    // At once, and its size beforehand.
    assert_eq!(
        document.copy_cells_now(1, 2, 1, 2).unwrap().as_deref(),
        Some("Marlow Foods\t\"say \"\"hi\"\"\tthere\"\nOstrava\t\"two\nlines marlow\"")
    );
    let estimate = document.estimated_copy_bytes(1, 2, 1, 2).unwrap();
    assert!((40..80).contains(&estimate), "{estimate}");
    // Waiting for a copy, as a pasteboard asking for it does.
    let waited = document.copy_cells(3, 1, 0, 1).unwrap();
    assert_eq!(waited.take_waiting(10_000).unwrap().as_deref(), Some("3"));
    assert_eq!(
        document.cell_value(2, 2, 6).unwrap(),
        Some(CellValue {
            text: "two\nli".to_owned(),
            truncated: true,
            characters: 16,
            lines: 2,
            invalid: false,
            exists: true,
        })
    );
    assert_eq!(document.cell_value(9, 0, 6).unwrap(), None);
}

/// DESIGN §3.9: once the document has failed, its search and copy say so
/// too.
#[test]
fn a_failed_document_fails_its_search_and_copy() {
    let dir = TempDir::new("find-failed");
    let scheduler = Scheduler::new().unwrap();
    let document = orders(&dir, &scheduler);
    let search = document.find("marlow".to_owned(), false).unwrap().unwrap();
    let copy = document.copy_cells(0, 1, 0, 1).unwrap();
    let _ = document.call(|| -> Result<(), LealError> { panic!("deliberate") });
    assert!(document.is_failed());
    let failed = |result: Result<(), LealError>| {
        assert!(
            matches!(result, Err(LealError::DocumentFailed { .. })),
            "{result:?}"
        );
    };
    failed(search.progress().map(|_| ()));
    failed(search.step(None, 0, true).map(|_| ()));
    failed(search.matches_in(0, 1, 0, 1, 1).map(|_| ()));
    failed(copy.take().map(|_| ()));
    failed(document.find("x".to_owned(), false).map(|_| ()));
    failed(document.cell_value(0, 0, 1).map(|_| ()));
}
