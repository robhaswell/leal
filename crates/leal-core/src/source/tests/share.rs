//! Tests for files on network shares (ADR-0009, PLAN 2.0): read like a
//! removable drive (ordinary reads, a copy streamed to the internal disk,
//! then a map of the copy), with a share's own rules on top:
//!
//! - `read_range` never reads the share, so nothing that may be on the main
//!   thread can block on it; only first paint (`read_head`) and the stream
//!   read it;
//! - network errors are retried with backoff, within a window, then the
//!   share counts as disconnected, as after any other failure on a share;
//! - `ENOENT` and `ESTALE` are decided by the path: deleted, replaced, or
//!   only disconnected;
//! - the share's file is closed on a thread of its own.
//!
//! There is no real share here. `Source::open_simulating_share` treats a
//! file in an ordinary temporary directory as on a share that can't clone,
//! and can make its reads slow or fail with a given error. The routing of a
//! real share (`MNT_LOCAL` missing, or a network file system's name) is
//! tested on its own (`the_removable_rule`, `network_file_systems_are_known_by_name`).

use super::removable::{contents, expected_chunks, kind, stream_all, stream_with};
use super::*;
use std::borrow::Cow;
use std::panic::{self, AssertUnwindSafe};
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

use crate::source::removable::PRETEND_MAIN_THREAD;
use leal_testkit::fidelity::{assert_identical, check_identical};

/// Retry waits short enough for tests: five, as in product, of 1 ms.
const QUICK_RETRIES: &[Duration] = &[Duration::from_millis(1); 5];

/// The network errors a share retries (ADR-0009, and the task 2.0
/// review's additions).
const NETWORK_ERRORS: [i32; 13] = [
    libc::ETIMEDOUT,
    libc::EHOSTDOWN,
    libc::EHOSTUNREACH,
    libc::ENETDOWN,
    libc::ENETUNREACH,
    libc::ECONNRESET,
    libc::ECONNREFUSED,
    libc::ECONNABORTED,
    libc::ENOTCONN,
    libc::EPIPE,
    libc::ESHUTDOWN,
    libc::EAGAIN,
    libc::EIO,
];

fn open_share(path: &Path, temp: &TempFolders, chunk_len: usize, share: SimulatedShare) -> Source {
    Source::open_simulating_share(path, temp, chunk_len, share).unwrap()
}

/// A share that answers at once, with quick retries.
fn quick_share() -> SimulatedShare {
    SimulatedShare {
        retry_delays: QUICK_RETRIES,
        ..SimulatedShare::default()
    }
}

/// A share whose reads from byte `at` fail with `errno`, `times` times
/// (`None`: until it is brought back).
fn failing_share(at: usize, errno: i32, times: Option<u32>) -> SimulatedShare {
    SimulatedShare {
        failure: Some(SimulatedShareFailure {
            at,
            errno,
            times,
            on_stat: false,
            partial: false,
        }),
        ..quick_share()
    }
}

/// As `failing_share`, but the `fstat` after the read fails.
fn failing_stat(at: usize, errno: i32, times: Option<u32>) -> SimulatedShare {
    let mut share = failing_share(at, errno, times);
    if let Some(failure) = &mut share.failure {
        failure.on_stat = true;
    }
    share
}

fn copied_len(source: &Source) -> usize {
    usize::try_from(source.available_len()).unwrap()
}

// ---------------------------------------------------------------------------
// Reading

/// A file on a share is read like one on a removable drive that can't
/// clone: the user's file itself, with ordinary reads, copied in chunks,
/// then the copy mapped. But only first paint and the stream read the
/// share; `read_range` gives `NotCopied` for what isn't copied yet.
#[test]
fn a_share_is_streamed_and_never_read_by_read_range() {
    let dir = TempDir::new("share-read");
    let bytes = contents(200_000);
    let path = dir.file("a.csv", &bytes);
    let temp = dir.temp_folders();
    let source = open_share(&path, &temp, 50_000, quick_share());

    assert_eq!(source.storage(), Storage::Reading);
    assert!(source.is_on_network_share());
    assert_eq!(source.as_slice(), None);
    assert!(source.can_save());
    assert_eq!(source.external_clone(), Some(path.clone()), "no clone");
    // Only the internal copy's folder: nothing was made on the share.
    assert_eq!(entries(temp.scratch()).len(), 1);

    // First paint reads the share.
    let head = source.read_head(64 * 1024).unwrap();
    assert!(matches!(head, Cow::Owned(_)));
    assert_eq!(&*head, &bytes[..64 * 1024]);
    assert_eq!(source.simulated_share_reads(), (1, 0));
    // `read_range` doesn't, even of the bytes first paint just read.
    for range in [0..10, 100_000..100_050, 0..200_000] {
        assert_eq!(
            kind(source.read_range(range.clone())),
            ReadErrorKind::NotCopied,
            "{range:?}"
        );
    }
    assert!(source.read_range(5..5).unwrap().is_empty());
    assert!(source.read_range(300_000..300_010).unwrap().is_empty());
    assert_eq!(source.simulated_share_reads(), (1, 0), "no more reads");

    // During the copy, what is copied can be read, and nothing past it.
    let streamed = stream_with(&source, &AtomicBool::new(false), |n, source| {
        let copied = (n + 1) * 50_000;
        assert_eq!(&*source.read_range(0..copied).unwrap(), &bytes[..copied]);
        if copied < bytes.len() {
            assert_eq!(
                kind(source.read_range(copied - 1..copied + 1)),
                ReadErrorKind::NotCopied
            );
        }
    });
    streamed.result.unwrap();
    assert_eq!(streamed.chunks, expected_chunks(bytes.len(), 50_000));
    assert_eq!(streamed.bytes, bytes);
    assert_eq!(source.simulated_share_reads(), (5, 0), "head and 4 chunks");

    // Then the copy is mapped, and the share isn't held any more.
    assert_eq!(source.storage(), Storage::Copy);
    assert_eq!(source.as_slice(), Some(bytes.as_slice()));
    assert!(matches!(
        source.read_range(0..10).unwrap(),
        Cow::Borrowed(_)
    ));
    assert!(source.is_on_network_share(), "still known to be a share");
    assert_eq!(source.external_clone(), None);
    assert!(source.can_save());
    assert_eq!(source.share_reads_on_main_thread(), 0);

    drop(source);
    assert!(entries(temp.scratch()).is_empty());
    assert!(entries(temp.records()).is_empty());
    assert_eq!(fs::read(&path).unwrap(), bytes, "the file is untouched");
}

/// A file shorter than first paint's 64 KB, and an empty one.
#[test]
fn a_short_or_empty_file_on_a_share() {
    let dir = TempDir::new("share-short");
    let temp = dir.temp_folders();
    let short = dir.file("short.csv", b"a,b\n1,2\n");
    let source = open_share(&short, &temp, 4, quick_share());
    assert_eq!(&*source.read_head(64 * 1024).unwrap(), b"a,b\n1,2\n");
    stream_all(&source).result.unwrap();
    assert_eq!(source.as_slice(), Some(&b"a,b\n1,2\n"[..]));

    let empty = dir.file("empty.csv", b"");
    let source = open_share(&empty, &temp, 4, quick_share());
    assert!(source.read_head(64 * 1024).unwrap().is_empty());
    assert_eq!(source.simulated_share_reads(), (0, 0), "nothing to read");
    stream_all(&source).result.unwrap();
    assert_eq!(source.storage(), Storage::Copy);
}

proptest! {
    /// F1 for the share path: for any bytes and chunk size, first paint's
    /// read, the stream's chunks, every `read_range` of the copied part
    /// during the copy, and the final map are all the file's bytes, and a
    /// Save As of the map would be byte-identical (`check_identical`, the
    /// testkit's oracle).
    #[test]
    fn any_bytes_read_back_identically_from_a_share(
        bytes in leal_testkit::strategies::bytes::csv_bytes(),
        chunk_len in 1_usize..64,
        cut in any::<prop::sample::Index>(),
    ) {
        let dir = TempDir::new("share-prop");
        let path = dir.file("a.csv", &bytes);
        let source = open_share(&path, &dir.temp_folders(), chunk_len, quick_share());
        let at = cut.index(bytes.len() + 1);
        prop_assert_eq!(&*source.read_head(at).unwrap(), &bytes[..at]);
        if at > 0 {
            prop_assert_eq!(kind(source.read_range(0..at)), ReadErrorKind::NotCopied);
        }

        let mut during = Ok(());
        let streamed = stream_with(&source, &AtomicBool::new(false), |n, source| {
            let copied = ((n + 1) * chunk_len).min(bytes.len());
            if during.is_ok() {
                during = check_identical(&bytes[..copied], &source.read_range(0..copied).unwrap());
            }
        });
        prop_assert!(streamed.result.is_ok());
        prop_assert_eq!(during, Ok(()));
        prop_assert_eq!(streamed.chunks, expected_chunks(bytes.len(), chunk_len));
        prop_assert_eq!(check_identical(&bytes, &streamed.bytes), Ok(()));
        prop_assert_eq!(check_identical(&bytes, source.as_slice().unwrap()), Ok(()));
    }
}

/// Every corpus file reads back byte for byte through a share, in odd-sized
/// chunks that cut rows, CRLFs, BOMs and multi-byte characters (F1).
#[test]
fn corpus_files_read_back_identically_from_a_share() {
    let dir = TempDir::new("share-corpus");
    let temp = dir.temp_folders();
    let cases = leal_testkit::corpus::load().unwrap();
    assert!(!cases.is_empty());
    for case in cases {
        let source = open_share(&case.path, &temp, 97, quick_share());
        let head = source.read_head(64 * 1024).unwrap();
        assert_eq!(&*head, &case.bytes[..head.len()], "{}", case.name);
        let streamed = stream_all(&source);
        streamed.result.unwrap();
        assert_identical(&case.bytes, &streamed.bytes);
        assert_identical(&case.bytes, source.as_slice().unwrap());
        assert_eq!(source.storage(), Storage::Copy, "{}", case.name);
    }
}

/// Another computer writes to the file while it is copied: as on a drive
/// that can't clone, the next chunk's check sees it, nothing read after it
/// is kept, and Save is refused. (A share's own caching can hide such a
/// change for a while; that limit is in the task notes.)
#[test]
fn a_change_on_the_share_while_copying_is_reported() {
    let dir = TempDir::new("share-change");
    let bytes = contents(40_000);
    let path = dir.file("a.csv", &bytes);
    let source = open_share(&path, &dir.temp_folders(), 10_000, quick_share());
    let streamed = stream_with(&source, &AtomicBool::new(false), |n, _| {
        if n == 1 {
            let mut longer = bytes.clone();
            longer.extend_from_slice(b"more\n");
            fs::write(&path, &longer).unwrap();
        }
    });
    assert_eq!(kind(streamed.result), ReadErrorKind::ChangedOnDisk);
    assert_eq!(streamed.chunks, expected_chunks(20_000, 10_000));
    assert!(source.changed_on_disk());
    assert!(!source.can_save());
    assert_eq!(source.as_slice(), None);
}

// ---------------------------------------------------------------------------
// Network errors: retried, then disconnected (ADR-0009)

/// Each network error, failing twice where the copy reaches the third
/// chunk: the read is tried again after a pause, the third try works, and
/// the copy completes as if nothing happened.
#[test]
fn network_errors_are_retried_and_the_copy_recovers() {
    let dir = TempDir::new("share-retry");
    let bytes = contents(50_000);
    let path = dir.file("a.csv", &bytes);
    for errno in NETWORK_ERRORS {
        let source = open_share(
            &path,
            &dir.temp_folders(),
            10_000,
            failing_share(25_000, errno, Some(2)),
        );
        let streamed = stream_all(&source);
        streamed.result.unwrap();
        assert_identical(&bytes, &streamed.bytes);
        assert_eq!(streamed.chunks, expected_chunks(bytes.len(), 10_000));
        assert_eq!(source.storage(), Storage::Copy, "errno {errno}");
        assert!(source.can_save());
        // Five chunks, and the third was tried twice more.
        assert_eq!(source.simulated_share_reads(), (7, 2), "errno {errno}");
    }
}

/// Each network error that doesn't pass: the read is retried after each
/// delay, then the share counts as disconnected, exactly as an unplugged
/// drive (ADR-0006): what was copied stays readable, nothing past it is,
/// and Save is refused. The waits really happen (backoff).
#[test]
fn network_errors_that_dont_pass_disconnect_the_share() {
    let dir = TempDir::new("share-retry-fail");
    let bytes = contents(50_000);
    let path = dir.file("a.csv", &bytes);
    const DELAYS: &[Duration] = &[Duration::from_millis(10), Duration::from_millis(20)];
    for errno in NETWORK_ERRORS {
        let source = open_share(
            &path,
            &dir.temp_folders(),
            10_000,
            SimulatedShare {
                retry_delays: DELAYS,
                ..failing_share(25_000, errno, None)
            },
        );
        let started = Instant::now();
        let streamed = stream_all(&source);
        let took = started.elapsed();
        let error = streamed.result.unwrap_err();
        assert_eq!(error.kind(), ReadErrorKind::Disconnected, "errno {errno}");
        assert_eq!(error.raw_os_error(), Some(errno), "the share's own error");
        assert!(took >= Duration::from_millis(30), "waited {took:?}");
        // Two chunks, then the third tried once and retried twice.
        assert_eq!(streamed.chunks, expected_chunks(20_000, 10_000));
        assert_eq!(source.simulated_share_reads(), (5, 3), "errno {errno}");

        assert_eq!(source.storage(), Storage::Disconnected);
        assert!(!source.can_save());
        assert_eq!(copied_len(&source), 20_000);
        assert_eq!(&*source.read_range(0..20_000).unwrap(), &bytes[..20_000]);
        assert_eq!(
            kind(source.read_range(19_000..21_000)),
            ReadErrorKind::Disconnected
        );
        // A new pass delivers what was copied, then stops at once.
        let again = stream_all(&source);
        assert_eq!(kind(again.result), ReadErrorKind::Disconnected);
        assert_eq!(again.chunks, expected_chunks(20_000, 10_000));
        assert_eq!(source.simulated_share_reads(), (5, 3), "no more reads");
    }
}

/// A share that stopped answering and is back: as for a drive (task 1.9),
/// `reconnect` reopens the unchanged file, and the copy carries on from
/// where it stopped. While it is away, it doesn't reconnect.
#[test]
fn a_disconnected_share_reconnects_when_it_answers_again() {
    let dir = TempDir::new("share-back");
    let bytes = contents(50_000);
    let path = dir.file("a.csv", &bytes);
    let source = open_share(
        &path,
        &dir.temp_folders(),
        10_000,
        failing_share(25_000, libc::ETIMEDOUT, None),
    );
    assert_eq!(
        kind(stream_all(&source).result),
        ReadErrorKind::Disconnected
    );
    assert!(!source.reconnect(&path), "still away");
    assert_eq!(source.storage(), Storage::Disconnected);

    source.simulate_drive_back();
    assert!(source.reconnect(&path));
    assert_eq!(source.storage(), Storage::Reading);
    assert!(source.can_save());
    let streamed = stream_all(&source);
    streamed.result.unwrap();
    assert_identical(&bytes, &streamed.bytes);
    assert_eq!(source.storage(), Storage::Copy);
}

/// A cancel stops a retry's wait at once, not when the wait is over.
#[test]
fn a_cancel_stops_the_wait_between_retries() {
    let dir = TempDir::new("share-retry-cancel");
    let path = dir.file("a.csv", &contents(30_000));
    const LONG_WAIT: &[Duration] = &[Duration::from_secs(60)];
    let source = open_share(
        &path,
        &dir.temp_folders(),
        10_000,
        SimulatedShare {
            retry_delays: LONG_WAIT,
            ..failing_share(15_000, libc::EHOSTDOWN, None)
        },
    );
    let cancel = AtomicBool::new(false);
    let started = Instant::now();
    let streamed = std::thread::scope(|scope| {
        scope.spawn(|| {
            std::thread::sleep(Duration::from_millis(50));
            cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        });
        stream_with(&source, &cancel, |_, _| {})
    });
    assert_eq!(kind(streamed.result), ReadErrorKind::Cancelled);
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_eq!(source.storage(), Storage::Reading, "not disconnected");
    assert_eq!(streamed.chunks, expected_chunks(10_000, 10_000));
}

/// First paint's read is retried too, off the main thread; one that never
/// works gives the disconnected error, which the app words as an open
/// error.
#[test]
fn first_paint_on_a_share_retries_network_errors() {
    let dir = TempDir::new("share-head");
    let bytes = contents(100_000);
    let path = dir.file("a.csv", &bytes);
    let temp = dir.temp_folders();
    let source = open_share(
        &path,
        &temp,
        10_000,
        failing_share(0, libc::ENETDOWN, Some(3)),
    );
    assert_eq!(&*source.read_head(64 * 1024).unwrap(), &bytes[..64 * 1024]);
    assert_eq!(source.simulated_share_reads(), (4, 3));

    let source = open_share(&path, &temp, 10_000, failing_share(0, libc::ENETDOWN, None));
    let error = source.read_head(64 * 1024).unwrap_err();
    assert_eq!(error.kind(), ReadErrorKind::Disconnected);
    assert_eq!(
        source.simulated_share_reads(),
        (6, 6),
        "tried once, retried 5 times"
    );
    assert_eq!(source.storage(), Storage::Disconnected);
}

/// Errors that aren't network errors aren't retried on a share, and any of
/// them makes the share disconnected, not an ordinary read error (task 2.0
/// review): `ENXIO`, `EBADF`, and even `EACCES` or `EINVAL`. The app keeps
/// checking for the share to come back.
#[test]
fn other_errors_on_a_share_disconnect_it_at_once() {
    let dir = TempDir::new("share-other");
    let path = dir.file("a.csv", &contents(30_000));
    for errno in [libc::ENXIO, libc::EBADF, libc::EACCES, libc::EINVAL] {
        let source = open_share(
            &path,
            &dir.temp_folders(),
            10_000,
            failing_share(15_000, errno, None),
        );
        let streamed = stream_all(&source);
        assert_eq!(
            kind(streamed.result),
            ReadErrorKind::Disconnected,
            "errno {errno}"
        );
        assert_eq!(
            source.simulated_share_reads(),
            (2, 1),
            "errno {errno}: once"
        );
        assert_eq!(source.storage(), Storage::Disconnected);
    }
}

/// The retries stop at the window, counting the tries themselves: a share
/// whose every try takes 40 ms gets 3 retries in a 100 ms window, not the 5
/// its delays allow.
#[test]
fn the_retries_stop_at_their_window() {
    let dir = TempDir::new("share-window");
    let path = dir.file("a.csv", &contents(30_000));
    let source = open_share(
        &path,
        &dir.temp_folders(),
        10_000,
        SimulatedShare {
            read_delay: Duration::from_millis(40),
            retry_window: Some(Duration::from_millis(100)),
            ..failing_share(15_000, libc::ETIMEDOUT, None)
        },
    );
    let started = Instant::now();
    let streamed = stream_all(&source);
    let took = started.elapsed();
    assert_eq!(kind(streamed.result), ReadErrorKind::Disconnected);
    let (reads, failed) = source.simulated_share_reads();
    // One chunk, then the failing chunk's tries: fewer than 1 + 5.
    assert!((2..6).contains(&failed), "{failed} tries of the chunk");
    assert_eq!(reads, 1 + failed);
    // The count of tries is the check; the time only guards against a
    // hang, so a busy machine doesn't fail it (task 2.G-b).
    assert!(took < Duration::from_secs(5), "took {took:?}");
}

/// A slow share's read takes its delay (`sys::sleep_strictly`, a kernel
/// timer in nanoseconds): never less, and not some other unit of it.
#[test]
fn a_slow_shares_read_takes_its_delay() {
    let dir = TempDir::new("share-delay");
    let path = dir.file("a.csv", &contents(30_000));
    let delay = Duration::from_millis(20);
    let source = open_share(
        &path,
        &dir.temp_folders(),
        10_000,
        SimulatedShare {
            read_delay: delay,
            hold_at: Some(0),
            ..quick_share()
        },
    );
    let started = Instant::now();
    source.read_head(64 * 1024).unwrap();
    let took = started.elapsed();
    assert!(took >= delay, "took {took:?}");
    // 20 s, if the delay were read in the wrong unit; a busy machine can
    // add a second or two (task 2.G-b).
    assert!(took < Duration::from_secs(5), "took {took:?}");
}

/// A failed `fstat` after a good read (`Attempt::Stat`): a network error is
/// retried and recovers; `ESTALE` with the file still there is only a
/// disconnection; anything else disconnects the share.
#[test]
fn a_failed_check_after_the_read_is_classified_too() {
    let dir = TempDir::new("share-stat");
    let bytes = contents(30_000);
    let path = dir.file("a.csv", &bytes);
    let source = open_share(
        &path,
        &dir.temp_folders(),
        10_000,
        failing_stat(15_000, libc::ETIMEDOUT, Some(2)),
    );
    let streamed = stream_all(&source);
    streamed.result.unwrap();
    assert_identical(&bytes, &streamed.bytes);
    assert_eq!(source.simulated_share_reads(), (5, 2), "two reads retried");

    for errno in [libc::ESTALE, libc::EACCES] {
        let source = open_share(
            &path,
            &dir.temp_folders(),
            10_000,
            failing_stat(15_000, errno, None),
        );
        let streamed = stream_all(&source);
        assert_eq!(
            kind(streamed.result),
            ReadErrorKind::Disconnected,
            "errno {errno}"
        );
        assert_eq!(streamed.chunks, expected_chunks(10_000, 10_000));
        assert_eq!(copied_len(&source), 10_000);
    }
}

/// A partial read followed by an error leaves no trace: the junk the failed
/// read put in the buffer is never delivered or copied, whether the retry
/// recovers or the share is disconnected.
#[test]
fn a_partial_read_then_an_error_leaves_no_trace() {
    let dir = TempDir::new("share-partial");
    let bytes = contents(30_000);
    let path = dir.file("a.csv", &bytes);
    for times in [Some(1), None] {
        let mut share = failing_share(15_000, libc::ECONNRESET, times);
        if let Some(failure) = &mut share.failure {
            failure.partial = true;
        }
        let source = open_share(&path, &dir.temp_folders(), 10_000, share);
        let streamed = stream_all(&source);
        // The junk (0xEE) never reaches the chunks or the copy.
        assert_identical(&bytes[..streamed.bytes.len()], &streamed.bytes);
        let copied = copied_len(&source);
        assert_identical(&bytes[..copied], &source.read_range(0..copied).unwrap());
        match times {
            Some(_) => {
                streamed.result.unwrap();
                assert_identical(&bytes, source.as_slice().unwrap());
            }
            None => {
                assert_eq!(kind(streamed.result), ReadErrorKind::Disconnected);
                assert_eq!(copied, 10_000);
            }
        }
    }
}

/// A short read of the user's file means it shrank, whatever `fstat` says
/// (a share's client may report the old size): a change, and Save is off.
/// Here the file itself is unchanged, and only the read was short.
#[test]
fn a_short_read_of_the_file_is_a_change() {
    let dir = TempDir::new("share-short-read");
    let bytes = contents(30_000);
    let path = dir.file("a.csv", &bytes);
    let source = open_share(
        &path,
        &dir.temp_folders(),
        10_000,
        failing_share(15_000, 0, None),
    );
    let streamed = stream_all(&source);
    assert_eq!(kind(streamed.result), ReadErrorKind::ChangedOnDisk);
    assert!(source.changed_on_disk());
    assert!(!source.can_save());
    assert_eq!(source.simulated_share_reads(), (2, 1), "not retried");
}

/// First paint's bytes are checked against the copy's: a same-size change
/// made between first paint and the copy, with its modification time put
/// back, passes the `fstat` check but not this one.
#[test]
fn a_change_between_first_paint_and_the_copy_is_caught() {
    let dir = TempDir::new("share-head-check");
    let bytes = contents(30_000);
    let path = dir.file("a.csv", &bytes);
    let source = open_share(&path, &dir.temp_folders(), 10_000, quick_share());
    assert_eq!(&*source.read_head(64 * 1024).unwrap(), bytes.as_slice());

    let modified = fs::metadata(&path).unwrap().modified().unwrap();
    let mut other = bytes.clone();
    other[5] ^= 0xFF;
    fs::write(&path, &other).unwrap();
    let file = OpenOptions::new().write(true).open(&path).unwrap();
    file.set_modified(modified).unwrap();
    drop(file);

    let streamed = stream_all(&source);
    assert_eq!(kind(streamed.result), ReadErrorKind::ChangedOnDisk);
    assert!(
        streamed.chunks.is_empty(),
        "the changed chunk isn't delivered"
    );
    assert!(source.changed_on_disk());
    assert!(!source.can_save());
}

/// The test hook that holds reads past an offset (for the app's tests):
/// the copy stops before it, and carries on once released.
#[test]
fn held_reads_wait_until_released() {
    let dir = TempDir::new("share-hold");
    let bytes = contents(50_000);
    let path = dir.file("a.csv", &bytes);
    let source = open_share(
        &path,
        &dir.temp_folders(),
        10_000,
        SimulatedShare {
            hold_at: Some(25_000),
            ..quick_share()
        },
    );
    std::thread::scope(|scope| {
        let streaming = scope.spawn(|| stream_all(&source));
        let deadline = Instant::now() + Duration::from_secs(10);
        while copied_bytes(&source) < 20_000 {
            assert!(Instant::now() < deadline, "the copy didn't get to the hold");
            std::thread::sleep(Duration::from_millis(5));
        }
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(copied_bytes(&source), 20_000, "held at the third chunk");
        source.simulated_share_release();
        let streamed = streaming.join().unwrap();
        streamed.result.unwrap();
        assert_identical(&bytes, &streamed.bytes);
    });
}

/// How much of the file the copy holds, while it is still reading.
fn copied_bytes(source: &Source) -> usize {
    (0..=5)
        .map(|chunks| chunks * 10_000)
        .take_while(|&end| end == 0 || source.read_range(0..end).is_ok())
        .last()
        .unwrap_or(0)
}

/// The names of the threads that closed a simulated share's file.
static CLOSED_ON: Mutex<Vec<Option<String>>> = Mutex::new(Vec::new());

fn record_close() {
    let name = std::thread::current().name().map(str::to_owned);
    CLOSED_ON.lock().unwrap().push(name);
}

/// Dropping a source on a share closes the share's file on a thread of its
/// own (task 2.0 review): a `close(2)` on a share that has stopped answering
/// can block, and the app may drop the document on the main thread.
#[test]
fn a_shares_file_is_closed_on_its_own_thread() {
    let dir = TempDir::new("share-close");
    let path = dir.file("a.csv", &contents(30_000));
    let source = open_share(
        &path,
        &dir.temp_folders(),
        10_000,
        SimulatedShare {
            on_close: Some(record_close),
            ..quick_share()
        },
    );
    let _ = source.read_head(1000).unwrap();
    drop(source);
    let deadline = Instant::now() + Duration::from_secs(10);
    while CLOSED_ON.lock().unwrap().is_empty() {
        assert!(Instant::now() < deadline, "the file was never closed");
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        *CLOSED_ON.lock().unwrap(),
        [Some("leal-share-close".to_owned())]
    );
}

// ---------------------------------------------------------------------------
// Deleted elsewhere (ADR-0009)

/// `ESTALE` and `ENOENT` from a share, with nothing at the file's path but
/// its folder still there: another computer deleted the file. That is
/// reported as the file deleted, not as a disconnection: the source is
/// `Deleted`, what was copied stays readable, Save is refused, nothing is
/// retried, and it never reconnects, even once the share answers again.
#[test]
fn estale_or_enoent_with_nothing_at_the_path_is_the_file_deleted() {
    let dir = TempDir::new("share-deleted");
    let bytes = contents(50_000);
    for errno in [libc::ESTALE, libc::ENOENT] {
        let path = dir.file("a.csv", &bytes);
        let source = open_share(
            &path,
            &dir.temp_folders(),
            10_000,
            failing_share(25_000, errno, None),
        );
        let streamed = stream_with(&source, &AtomicBool::new(false), |n, _| {
            if n == 1 {
                fs::remove_file(&path).unwrap();
            }
        });
        let error = streamed.result.unwrap_err();
        assert_eq!(error.kind(), ReadErrorKind::Deleted, "errno {errno}");
        assert_eq!(error.raw_os_error(), Some(errno));
        assert_eq!(streamed.chunks, expected_chunks(20_000, 10_000));
        assert_eq!(source.simulated_share_reads(), (3, 1), "not retried");

        assert_eq!(source.storage(), Storage::Deleted, "errno {errno}");
        assert_ne!(source.storage(), Storage::Disconnected);
        assert!(!source.can_save());
        assert_eq!(copied_len(&source), 20_000);
        assert_eq!(&*source.read_range(0..20_000).unwrap(), &bytes[..20_000]);
        assert_eq!(
            kind(source.read_range(15_000..25_000)),
            ReadErrorKind::Deleted
        );
        let again = stream_all(&source);
        assert_eq!(kind(again.result), ReadErrorKind::Deleted);
        assert_eq!(again.chunks, expected_chunks(20_000, 10_000));

        source.simulate_drive_back();
        assert!(!source.reconnect(&path), "a deleted file isn't reopened");
        assert_eq!(source.storage(), Storage::Deleted);
    }
}

/// `ESTALE` with the file still at its path, the same file: only the
/// share's handle went stale (an NFS server that restarted). Disconnected,
/// and it reconnects and finishes the copy once the share answers.
#[test]
fn estale_with_the_same_file_at_the_path_is_a_disconnection() {
    let dir = TempDir::new("share-stale-handle");
    let bytes = contents(50_000);
    let path = dir.file("a.csv", &bytes);
    let source = open_share(
        &path,
        &dir.temp_folders(),
        10_000,
        failing_share(25_000, libc::ESTALE, None),
    );
    let streamed = stream_all(&source);
    assert_eq!(kind(streamed.result), ReadErrorKind::Disconnected);
    assert_eq!(source.storage(), Storage::Disconnected);
    source.simulate_drive_back();
    assert!(source.reconnect(&path));
    let again = stream_all(&source);
    again.result.unwrap();
    assert_identical(&bytes, &again.bytes);
}

/// `ESTALE` with another file at the path: the file was replaced (a save
/// on another computer). Changed while reading, so the app offers Reload.
#[test]
fn estale_with_another_file_at_the_path_is_a_change() {
    let dir = TempDir::new("share-replaced");
    let bytes = contents(50_000);
    let path = dir.file("a.csv", &bytes);
    let source = open_share(
        &path,
        &dir.temp_folders(),
        10_000,
        failing_share(25_000, libc::ESTALE, None),
    );
    let streamed = stream_with(&source, &AtomicBool::new(false), |n, _| {
        if n == 1 {
            let replacement = dir.path().join("new.csv");
            fs::write(&replacement, b"another,file\n").unwrap();
            fs::rename(&replacement, &path).unwrap();
        }
    });
    assert_eq!(kind(streamed.result), ReadErrorKind::ChangedOnDisk);
    assert!(source.changed_on_disk());
    assert!(!source.can_save());
    assert_ne!(source.storage(), Storage::Deleted);
}

/// `ESTALE` with nothing at the path and its folder gone too: the share
/// itself has gone (or the folder can't be seen). Disconnected, not deleted.
#[test]
fn estale_with_the_folder_gone_is_a_disconnection() {
    let dir = TempDir::new("share-folder-gone");
    let bytes = contents(50_000);
    let folder = dir.folder("on-share");
    let path = folder.join("a.csv");
    fs::write(&path, &bytes).unwrap();
    let source = open_share(
        &path,
        &dir.temp_folders(),
        10_000,
        failing_share(25_000, libc::ESTALE, None),
    );
    let streamed = stream_with(&source, &AtomicBool::new(false), |n, _| {
        if n == 1 {
            fs::remove_dir_all(&folder).unwrap();
        }
    });
    assert_eq!(kind(streamed.result), ReadErrorKind::Disconnected);
    assert_eq!(source.storage(), Storage::Disconnected);
}

/// On a removable drive (not a share), `ENOENT` is still a disconnection,
/// and network errors aren't retried: the share rules are the share's only.
#[test]
fn a_removable_drive_keeps_its_own_rules() {
    let dir = TempDir::new("share-vs-drive");
    let bytes = contents(30_000);
    let path = dir.file("a.csv", &bytes);
    let source = Source::open_simulating_removable(&path, &dir.temp_folders(), 10_000).unwrap();
    assert!(!source.is_on_network_share());
    // `read_range` still reads the drive, and `read_head` is the same.
    assert_eq!(&*source.read_range(0..1000).unwrap(), &bytes[..1000]);
    assert_eq!(&*source.read_head(1000).unwrap(), &bytes[..1000]);
    // And the main-thread rule is the share's only.
    let on_main = PRETEND_MAIN_THREAD.with(|pretend| {
        pretend.set(true);
        let read = source.read_head(1000).map(Cow::into_owned);
        pretend.set(false);
        read
    });
    assert_eq!(on_main.unwrap(), &bytes[..1000]);
    assert_eq!(source.share_reads_on_main_thread(), 0);
}

// ---------------------------------------------------------------------------
// Never on the main thread (ADR-0009)

/// The share must never be read on the main thread. Tests never run on the
/// process's main thread, so this one pretends: a read of the share there
/// (first paint, or a stream) is counted, and a debug build panics. Reads
/// of the copy, and `read_range`, aren't reads of the share, so they are
/// fine there. The app's tests check that the count stays 0
/// (`NetworkShareTests`).
#[test]
fn a_read_of_the_share_on_the_main_thread_is_caught() {
    let dir = TempDir::new("share-main");
    let bytes = contents(30_000);
    let path = dir.file("a.csv", &bytes);
    let source = open_share(&path, &dir.temp_folders(), 10_000, quick_share());
    let on_main = |read: &dyn Fn()| {
        PRETEND_MAIN_THREAD.with(|pretend| pretend.set(true));
        let result = panic::catch_unwind(AssertUnwindSafe(read));
        PRETEND_MAIN_THREAD.with(|pretend| pretend.set(false));
        result.is_err()
    };

    // Off the main thread, nothing is counted.
    assert_eq!(&*source.read_head(1000).unwrap(), &bytes[..1000]);
    assert_eq!(source.share_reads_on_main_thread(), 0);

    // `read_range` on the main thread never reads the share.
    let panicked = on_main(&|| {
        assert_eq!(kind(source.read_range(0..1000)), ReadErrorKind::NotCopied);
    });
    assert!(!panicked);
    assert_eq!(source.share_reads_on_main_thread(), 0);

    // First paint on the main thread is caught.
    let panicked = on_main(&|| {
        let _ = source.read_head(1000);
    });
    assert_eq!(source.share_reads_on_main_thread(), 1);
    assert_eq!(panicked, cfg!(debug_assertions), "debug builds panic");

    // So is a reconnection (it reopens the file).
    let panicked = on_main(&|| {
        let _ = source.reconnect(&path);
    });
    assert_eq!(source.share_reads_on_main_thread(), 2);
    assert_eq!(panicked, cfg!(debug_assertions));

    // The copy completes off the main thread, and then the main thread
    // reads only the map.
    stream_all(&source).result.unwrap();
    let panicked = on_main(&|| {
        assert_eq!(&*source.read_range(0..30_000).unwrap(), bytes.as_slice());
        assert_eq!(&*source.read_head(1000).unwrap(), &bytes[..1000]);
    });
    assert!(!panicked);
    assert_eq!(source.share_reads_on_main_thread(), 2);

    // Opening a file on a share on the main thread is caught too.
    let panicked = on_main(&|| {
        let _ = open_share(&path, &dir.temp_folders(), 10_000, quick_share());
    });
    assert_eq!(panicked, cfg!(debug_assertions));
}

/// `ESTALE` with nothing at the path and its folder on another volume: an
/// unmounted share leaves its empty mount point where a file at its root
/// was. The share has gone, so it is a disconnection, not a deletion (task
/// 2.0 re-review). The folder here becomes a symbolic link to `/dev`, which
/// is on another volume (`devfs`).
#[test]
fn estale_with_the_folder_on_another_volume_is_a_disconnection() {
    let dir = TempDir::new("share-mount-point");
    let bytes = contents(50_000);
    let folder = dir.folder("mounted");
    let path = folder.join("a.csv");
    fs::write(&path, &bytes).unwrap();
    assert_ne!(
        fs::metadata("/dev").unwrap().dev(),
        fs::metadata(&folder).unwrap().dev(),
        "the test needs /dev on another volume"
    );
    let source = open_share(
        &path,
        &dir.temp_folders(),
        10_000,
        failing_share(25_000, libc::ESTALE, None),
    );
    let streamed = stream_with(&source, &AtomicBool::new(false), |n, _| {
        if n == 1 {
            fs::remove_dir_all(&folder).unwrap();
            std::os::unix::fs::symlink("/dev", &folder).unwrap();
        }
    });
    assert_eq!(kind(streamed.result), ReadErrorKind::Disconnected);
    assert_eq!(source.storage(), Storage::Disconnected);
}

/// A file moved since it was opened: the look after `ESTALE` is at where it
/// is now, as the watcher reports it (`note_original_path`), so the same
/// file there is a stale handle, not a deletion at the old path.
#[test]
fn estale_looks_at_where_the_file_is_now() {
    let dir = TempDir::new("share-moved");
    let bytes = contents(50_000);
    let path = dir.file("a.csv", &bytes);
    let moved = dir.path().join("b.csv");
    let source = open_share(
        &path,
        &dir.temp_folders(),
        10_000,
        failing_share(25_000, libc::ESTALE, None),
    );
    let streamed = stream_with(&source, &AtomicBool::new(false), |n, source| {
        if n == 1 {
            fs::rename(&path, &moved).unwrap();
            source.note_original_path(&moved);
        }
    });
    assert_eq!(kind(streamed.result), ReadErrorKind::Disconnected);
    assert_ne!(source.storage(), Storage::Deleted);
}

/// A cancel stops a read held by the hold hook, as it stops a retry's wait
/// (task 2.0 re-review).
#[test]
fn a_cancel_stops_a_held_read() {
    let dir = TempDir::new("share-hold-cancel");
    let path = dir.file("a.csv", &contents(50_000));
    let source = open_share(
        &path,
        &dir.temp_folders(),
        10_000,
        SimulatedShare {
            hold_at: Some(25_000),
            ..quick_share()
        },
    );
    let cancel = AtomicBool::new(false);
    let started = Instant::now();
    let streamed = std::thread::scope(|scope| {
        scope.spawn(|| {
            std::thread::sleep(Duration::from_millis(50));
            cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        });
        stream_with(&source, &cancel, |_, _| {})
    });
    assert_eq!(kind(streamed.result), ReadErrorKind::Cancelled);
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_eq!(streamed.chunks, expected_chunks(20_000, 10_000));
}

/// With Leal's temporary folder on a network volume (a network home
/// folder), the complete copy isn't mapped: it is read with `pread`, like
/// the part copied before, because a vanished share would make touching a
/// map crash the process (task 2.0 re-review). Everything else is as for a
/// mapped copy.
#[test]
fn a_copy_on_a_network_volume_is_read_not_mapped() {
    let dir = TempDir::new("copy-on-network");
    let bytes = contents(30_000);
    let path = dir.file("a.csv", &bytes);
    let source = Source::open_with_options(
        &path,
        &dir.temp_folders(),
        VolumeInfo::default(),
        Options {
            volume: VolumeCheck::Removable,
            chunk_len: 10_000,
            copy_on_network: true,
            ..Options::default()
        },
    )
    .unwrap();
    let streamed = stream_all(&source);
    streamed.result.unwrap();
    assert_identical(&bytes, &streamed.bytes);
    assert_eq!(source.storage(), Storage::Copy);
    assert_eq!(source.as_slice(), None, "not mapped");
    assert!(source.can_save());
    let read = source.read_range(0..30_000).unwrap();
    assert!(
        matches!(read, Cow::Owned(_)),
        "read, not borrowed from a map"
    );
    assert_identical(&bytes, &read);
    assert_eq!(source.external_clone(), None, "the drive's file is let go");
    // Another pass reads the copy.
    let again = stream_all(&source);
    again.result.unwrap();
    assert_identical(&bytes, &again.bytes);
}
