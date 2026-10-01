//! The whole-file review (P2) and first paint from the head alone.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use common::{len, review};
use leal_core::detect::{Cancelled, Choices, FIRST_PAINT_BYTES, Hints, REVIEW_CHUNK_BYTES, detect};
use leal_core::dialect::{Delimiter, LineEnding};

fn plain(bytes: &[u8]) -> leal_core::detect::Detection {
    common::detect(bytes, Hints::default(), Choices::default()).unwrap()
}

/// `body` repeated until the file is at least `size` bytes.
fn repeated(header: &[u8], body: &[u8], size: usize) -> Vec<u8> {
    let mut file = header.to_vec();
    while file.len() < size {
        file.extend_from_slice(body);
    }
    file
}

/// Multi-line quoted fields full of commas. A review that started reading
/// in the middle of a field would see the quotes the wrong way round and
/// find commas outside them; reading from the start, it never does.
#[test]
fn multi_line_quoted_fields_get_no_delimiter_suggestion() {
    let file = repeated(
        b"id;notes;n\n",
        b"0;\"first line, a\nsecond line, b\n\";0\n",
        1 << 20,
    );
    let d = plain(&file);
    assert_eq!(d.delimiter, Delimiter::Semicolon);
    let r = review(&file, &d);
    assert_eq!(r.delimiter_suggestion, None);
    assert_eq!(r.line_ending, Some(LineEnding::Lf));
    assert!(!r.mixed_line_endings);
    assert!(r.trailing_newline);
}

/// A wide matrix whose first row is longer than 64 KB: first paint has only
/// that cut row to go on, and uses it.
#[test]
fn a_first_row_longer_than_64_kb_still_decides_the_delimiter() {
    for d in [Delimiter::Tab, Delimiter::Semicolon, Delimiter::Pipe] {
        let mut row: Vec<u8> = Vec::new();
        for i in 0..20_000 {
            if i > 0 {
                row.push(d.byte());
            }
            row.extend_from_slice(format!("{i}").as_bytes());
        }
        row.push(b'\n');
        assert!(row.len() > FIRST_PAINT_BYTES);
        let file = row.repeat(3);
        let first = plain(&file);
        assert_eq!(first.delimiter, d);
        assert_eq!(first.line_ending, None); // none seen in the first 64 KB
        let r = review(&file, &first);
        assert_eq!(r.delimiter_suggestion, None);
        assert_eq!(r.line_ending, Some(LineEnding::Lf));
    }
}

/// The caller may pass only the first 64 KB, with the file's length.
#[test]
fn first_paint_from_the_head_alone_is_the_same() {
    let file = repeated(b"a;b\n", b"1;\"x\ny\"\r\n", 5 * FIRST_PAINT_BYTES);
    let whole = detect(&file, len(&file), Hints::default(), Choices::default()).unwrap();
    let head = detect(
        &file[..FIRST_PAINT_BYTES],
        len(&file),
        Hints::default(),
        Choices::default(),
    )
    .unwrap();
    assert_eq!(head, whole);
    assert_eq!(head.trailing_newline, None);
    // Told the head is the whole file, first paint knows the trailing
    // newline of what it was given.
    let short = &file[..FIRST_PAINT_BYTES];
    let alone = detect(short, len(short), Hints::default(), Choices::default()).unwrap();
    assert!(alone.trailing_newline.is_some());
}

#[test]
fn a_cancelled_review_stops() {
    let file = repeated(b"a,b\n", b"1,2\n", 3 * REVIEW_CHUNK_BYTES);
    let d = plain(&file);
    let cancel = AtomicBool::new(true);
    assert_eq!(
        leal_core::detect::review(&file, &d, &cancel),
        Err(Cancelled)
    );
    // Nothing to read: nothing to cancel.
    let empty = plain(b"");
    assert!(leal_core::detect::review(b"", &empty, &cancel).is_ok());
}

/// Cancelling from another thread while the review runs stops it at its
/// next chunk.
#[test]
fn a_review_can_be_cancelled_while_it_runs() {
    let file = repeated(b"a,b\n", b"1,\"2\n3\"\n", 256 * REVIEW_CHUNK_BYTES);
    let d = plain(&file);
    let cancel = Arc::new(AtomicBool::new(false));
    let setter = {
        let cancel = Arc::clone(&cancel);
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(5));
            cancel.store(true, Ordering::Relaxed);
        })
    };
    let result = leal_core::detect::review(&file, &d, &cancel);
    setter.join().unwrap();
    assert_eq!(result, Err(Cancelled));
}
