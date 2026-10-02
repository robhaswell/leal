//! Tests for files on removable drives (ADR-0006 option C, PLAN 1.1a): read
//! with ordinary reads, never mapped, copied to the internal disk in
//! chunks, then mapped from there; and the "drive disconnected" state.
//!
//! Most tests force the removable path on a file in an ordinary temporary
//! directory (`VolumeCheck::Removable`), so they run fast and in parallel.
//! The ones that need a volume to vanish use a disk image and
//! `hdiutil detach -force`, and are in the `disk-images` test group.

use super::*;
use std::borrow::Cow;
use std::sync::atomic::AtomicBool;

/// `n` bytes that differ from position to position, so a chunk delivered at
/// the wrong offset can't compare equal.
pub(super) fn contents(n: usize) -> Vec<u8> {
    (0..n).map(|i| u8::try_from(i % 251).unwrap()).collect()
}

/// Opens `path` as if it were on a removable volume, streaming in chunks of
/// `chunk_len` bytes.
fn open_removable(path: &Path, temp: &TempFolders, chunk_len: usize) -> Source {
    Source::open_with_options(
        path,
        temp,
        VolumeInfo::default(),
        Options {
            volume: VolumeCheck::Removable,
            chunk_len,
            ..Options::default()
        },
    )
    .unwrap()
}

/// What one `stream` pass delivered.
pub(super) struct Streamed {
    /// Each chunk's offset and length, in order.
    pub(super) chunks: Vec<(usize, usize)>,
    /// The chunks' bytes, concatenated.
    pub(super) bytes: Vec<u8>,
    pub(super) result: Result<(), ReadError>,
}

/// Runs `stream` to the end, calling `during(chunk number, source)` after
/// each chunk.
pub(super) fn stream_with(
    source: &Source,
    cancel: &AtomicBool,
    mut during: impl FnMut(usize, &Source),
) -> Streamed {
    let mut chunks = Vec::new();
    let mut bytes = Vec::new();
    let result = source.stream(cancel, |chunk| {
        chunks.push((chunk.offset, chunk.bytes.len()));
        bytes.extend_from_slice(chunk.bytes);
        during(chunks.len() - 1, source);
    });
    Streamed {
        chunks,
        bytes,
        result,
    }
}

pub(super) fn stream_all(source: &Source) -> Streamed {
    stream_with(source, &AtomicBool::new(false), |_, _| {})
}

/// The chunks a pass over `len` bytes in chunks of `chunk_len` delivers.
pub(super) fn expected_chunks(len: usize, chunk_len: usize) -> Vec<(usize, usize)> {
    (0..len)
        .step_by(chunk_len)
        .map(|offset| (offset, chunk_len.min(len - offset)))
        .collect()
}

/// The whole file through `read_range`.
fn read_all(source: &Source) -> Vec<u8> {
    let len = usize::try_from(source.len()).unwrap();
    source.read_range(0..len).unwrap().into_owned()
}

pub(super) fn kind<T: fmt::Debug>(result: Result<T, ReadError>) -> ReadErrorKind {
    result.unwrap_err().kind()
}

fn mode(path: &Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}

// ---------------------------------------------------------------------------
// Reading before and during the copy

#[test]
fn a_removable_source_is_read_with_ordinary_reads() {
    let dir = TempDir::new("removable-read");
    let bytes = contents(200_000);
    let path = dir.file("a.csv", &bytes);
    let temp = dir.temp_folders();
    let source = open_removable(&path, &temp, STREAM_CHUNK_BYTES);

    assert_eq!(source.storage(), Storage::Reading);
    assert_eq!(source.as_slice(), None, "nothing is mapped yet");
    assert_eq!(source.len(), 200_000);
    assert_eq!(source.available_len(), 200_000);
    assert!(source.can_save());

    // First paint: the first 64 KB.
    let first = source.read_range(0..64 * 1024).unwrap();
    assert!(
        matches!(first, Cow::Owned(_)),
        "read, not borrowed from a map"
    );
    assert_eq!(&*first, &bytes[..64 * 1024]);
    // A row in the middle.
    assert_eq!(
        &*source.read_range(100_003..100_050).unwrap(),
        &bytes[100_003..100_050]
    );
    assert_eq!(read_all(&source), bytes);
    // Ranges are clamped to the file, like `pread`.
    assert_eq!(
        &*source.read_range(199_990..300_000).unwrap(),
        &bytes[199_990..]
    );
    assert!(source.read_range(250_000..260_000).unwrap().is_empty());
    #[expect(clippy::reversed_empty_ranges, reason = "the case under test")]
    let backwards = 10..5;
    assert!(source.read_range(backwards).unwrap().is_empty());

    // The external clone and the internal copy, each in a recorded folder.
    // (Here both are in the scratch directory: the volume is only
    // pretending to be removable.)
    assert_eq!(entries(temp.scratch()).len(), 2);
    assert_eq!(entries(temp.records()).len(), 2);
}

#[test]
fn reads_work_while_the_copy_runs() {
    let dir = TempDir::new("removable-during");
    let bytes = contents(10_500);
    let path = dir.file("a.csv", &bytes);
    let source = open_removable(&path, &dir.temp_folders(), 1000);

    let streamed = stream_with(&source, &AtomicBool::new(false), |n, source| {
        // Part of the file is copied and part isn't: reads see all of it.
        assert_eq!(source.storage(), Storage::Reading, "chunk {n}");
        assert_eq!(read_all(source), bytes, "chunk {n}");
        // A range that straddles the copied part's end.
        let end = ((n + 1) * 1000).min(bytes.len());
        assert_eq!(
            &*source.read_range(end - 10..end + 10).unwrap(),
            &bytes[end - 10..(end + 10).min(bytes.len())],
            "chunk {n}"
        );
    });
    streamed.result.unwrap();
}

// ---------------------------------------------------------------------------
// The copy, and the switch to the internal map

#[test]
fn the_copy_streams_in_chunks_then_the_internal_copy_is_mapped() {
    let dir = TempDir::new("removable-copy");
    let bytes = contents(10_500);
    let path = dir.file("a.csv", &bytes);
    let temp = dir.temp_folders();
    let source = open_removable(&path, &temp, 1000);
    let external = source.external_clone().unwrap();
    let internal = source.temp.as_ref().unwrap().file_path();
    assert!(external.exists());

    let streamed = stream_all(&source);
    streamed.result.unwrap();
    assert_eq!(streamed.chunks, expected_chunks(10_500, 1000));
    assert_eq!(streamed.bytes, bytes);

    // Switched to the map of the internal copy...
    assert_eq!(source.storage(), Storage::Copy);
    assert_eq!(source.as_slice(), Some(bytes.as_slice()));
    let borrowed = source.read_range(5..50).unwrap();
    assert!(
        matches!(borrowed, Cow::Borrowed(_)),
        "borrowed from the map"
    );
    assert_eq!(&*borrowed, &bytes[5..50]);
    assert_eq!(mode(&internal), 0o400);
    // ...and the external clone, its folder and its record are gone.
    assert_eq!(source.external_clone(), None);
    assert!(!external.exists());
    assert!(!external.parent().unwrap().exists());
    assert_eq!(entries(temp.scratch()), vec![internal.parent().unwrap()]);
    assert_eq!(entries(temp.records()).len(), 1);

    // Streaming again (re-indexing with another delimiter) reads the map.
    let again = stream_all(&source);
    again.result.unwrap();
    assert_eq!(again.chunks, expected_chunks(10_500, 1000));
    assert_eq!(again.bytes, bytes);

    drop(source);
    assert!(entries(temp.scratch()).is_empty());
    assert!(entries(temp.records()).is_empty());
}

/// The copy is of the clone, so it is a snapshot of the file at open, even
/// if the original changes while it runs.
#[test]
fn the_copy_is_a_snapshot() {
    let dir = TempDir::new("removable-snapshot");
    let bytes = contents(5000);
    let path = dir.file("a.csv", &bytes);
    let source = open_removable(&path, &dir.temp_folders(), 1000);
    fs::write(&path, b"changed").unwrap();
    let streamed = stream_with(&source, &AtomicBool::new(false), |_, _| {
        fs::write(&path, b"changed again").unwrap();
    });
    streamed.result.unwrap();
    assert_eq!(streamed.bytes, bytes);
    assert_eq!(source.as_slice(), Some(bytes.as_slice()));
    assert!(!source.changed_on_disk(), "a clone can't change");
}

#[test]
fn an_empty_file_on_a_removable_volume() {
    let dir = TempDir::new("removable-empty");
    let path = dir.file("empty.csv", b"");
    let temp = dir.temp_folders();
    let source = open_removable(&path, &temp, 1000);
    assert_eq!(source.storage(), Storage::Reading);
    assert!(source.read_range(0..10).unwrap().is_empty());

    let streamed = stream_all(&source);
    streamed.result.unwrap();
    assert!(streamed.chunks.is_empty());
    assert_eq!(source.storage(), Storage::Copy);
    assert_eq!(source.as_slice(), Some(&b""[..]));
    assert_eq!(entries(temp.scratch()).len(), 1);
}

/// Internal volumes stream straight from the map (or memory): nothing is
/// copied, and the storage doesn't change.
#[test]
fn mapped_sources_stream_without_copying() {
    let dir = TempDir::new("stream-mapped");
    let bytes = contents(2500);
    let path = dir.file("a.csv", &bytes);
    let temp = dir.temp_folders();
    let source = Source::open_with_options(
        &path,
        &temp,
        VolumeInfo::default(),
        Options {
            chunk_len: 1000,
            ..Options::default()
        },
    )
    .unwrap();
    assert_eq!(source.storage(), Storage::Clone);

    let streamed = stream_all(&source);
    streamed.result.unwrap();
    assert_eq!(streamed.chunks, vec![(0, 1000), (1000, 1000), (2000, 500)]);
    assert_eq!(streamed.bytes, bytes);
    assert_eq!(source.storage(), Storage::Clone);
    assert_eq!(entries(temp.scratch()).len(), 1);
    assert!(matches!(
        source.read_range(0..10).unwrap(),
        Cow::Borrowed(_)
    ));

    let cancel = AtomicBool::new(true);
    let cancelled = stream_with(&source, &cancel, |_, _| {});
    assert_eq!(kind(cancelled.result), ReadErrorKind::Cancelled);
    assert!(cancelled.chunks.is_empty());
}

proptest! {
    /// Any bytes read back identically from a removable volume, through
    /// ordinary reads, through the stream (whatever the chunk size), and
    /// through the map afterwards (F1 for the removable path).
    #[test]
    fn any_bytes_read_back_identically_from_a_removable_volume(
        bytes in leal_testkit::strategies::bytes::csv_bytes(),
        chunk_len in 1_usize..64,
        cut in any::<prop::sample::Index>(),
    ) {
        let dir = TempDir::new("removable-prop");
        let path = dir.file("a.csv", &bytes);
        let source = open_removable(&path, &dir.temp_folders(), chunk_len);
        let at = cut.index(bytes.len() + 1);
        prop_assert_eq!(&*source.read_range(0..at).unwrap(), &bytes[..at]);
        prop_assert_eq!(&*source.read_range(at..bytes.len()).unwrap(), &bytes[at..]);

        let streamed = stream_all(&source);
        prop_assert!(streamed.result.is_ok());
        prop_assert_eq!(streamed.chunks, expected_chunks(bytes.len(), chunk_len));
        prop_assert_eq!(&streamed.bytes, &bytes);
        prop_assert_eq!(source.as_slice(), Some(bytes.as_slice()));
    }
}

// ---------------------------------------------------------------------------
// Cancelling

#[test]
fn cancelling_the_copy_stops_at_the_next_chunk() {
    let dir = TempDir::new("removable-cancel");
    let bytes = contents(10_500);
    let path = dir.file("a.csv", &bytes);
    let temp = dir.temp_folders();
    let source = open_removable(&path, &temp, 1000);

    let cancel = AtomicBool::new(false);
    let streamed = stream_with(&source, &cancel, |n, _| {
        if n == 2 {
            cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        }
    });
    assert_eq!(kind(streamed.result), ReadErrorKind::Cancelled);
    assert_eq!(
        streamed.chunks,
        expected_chunks(3000, 1000),
        "no fourth chunk"
    );

    // Still reading from the external clone, and still readable.
    assert_eq!(source.storage(), Storage::Reading);
    assert_eq!(read_all(&source), bytes);
    assert!(source.can_save());
    assert_eq!(entries(temp.scratch()).len(), 2);

    // A new pass starts from the beginning again (the copied part comes
    // from the internal copy) and completes.
    let again = stream_all(&source);
    again.result.unwrap();
    assert_eq!(again.chunks, expected_chunks(10_500, 1000));
    assert_eq!(again.bytes, bytes);
    assert_eq!(source.storage(), Storage::Copy);
    assert_eq!(entries(temp.scratch()).len(), 1);
}

#[test]
fn cancelling_before_the_first_chunk_delivers_nothing() {
    let dir = TempDir::new("removable-cancel-first");
    let path = dir.file("a.csv", &contents(5000));
    let source = open_removable(&path, &dir.temp_folders(), 1000);
    let streamed = stream_with(&source, &AtomicBool::new(true), |_, _| {});
    assert_eq!(kind(streamed.result), ReadErrorKind::Cancelled);
    assert!(streamed.chunks.is_empty());
    assert_eq!(source.storage(), Storage::Reading);
}

// ---------------------------------------------------------------------------
// Cleanup

#[test]
fn dropping_a_removable_source_mid_copy_removes_both_folders() {
    let dir = TempDir::new("removable-drop");
    let path = dir.file("a.csv", &contents(5000));
    let temp = dir.temp_folders();
    let source = open_removable(&path, &temp, 1000);
    let cancel = AtomicBool::new(false);
    let streamed = stream_with(&source, &cancel, |_, _| {
        cancel.store(true, std::sync::atomic::Ordering::Relaxed);
    });
    assert_eq!(kind(streamed.result), ReadErrorKind::Cancelled);
    assert_eq!(entries(temp.scratch()).len(), 2);
    assert_eq!(entries(temp.records()).len(), 2);

    drop(source);
    assert!(entries(temp.scratch()).is_empty());
    assert!(entries(temp.records()).is_empty());
    assert!(path.exists());
}

#[test]
fn a_crash_mid_copy_leaves_both_folders_for_cleanup() {
    let dir = TempDir::new("removable-crash");
    let path = dir.file("a.csv", &contents(5000));
    let temp = dir.temp_folders();
    let source = open_removable(&path, &temp, 1000);
    let cancel = AtomicBool::new(false);
    let _ = stream_with(&source, &cancel, |_, _| {
        cancel.store(true, std::sync::atomic::Ordering::Relaxed);
    });
    crash(source);
    assert_eq!(entries(temp.scratch()).len(), 2);

    assert_eq!(temp.remove_leftovers().unwrap(), 2);
    assert!(entries(temp.scratch()).is_empty());
    assert!(entries(temp.records()).is_empty());
}

// ---------------------------------------------------------------------------
// Detection

/// The rule (PLAN 1.1a): a local volume is removable unless it is known to
/// be internal and not ejectable. Either Foundation's facts or the volume's
/// own mount flags saying it is removable is enough. A network share is
/// its own kind (ADR-0009), by its mount flags or its file system's name.
#[test]
fn the_removable_rule() {
    use volume::VolumeKind::{Fixed, Network, Removable};
    let facts = |is_internal, is_ejectable| VolumeInfo {
        folder: None,
        is_internal,
        is_ejectable,
    };
    let internal_flags = VolumeFlags {
        local: true,
        removable: false,
        network_type: false,
    };
    for (info, kind) in [
        (facts(Some(true), Some(false)), Fixed),
        (facts(Some(true), None), Fixed),
        (facts(Some(true), Some(true)), Removable),
        (facts(Some(false), Some(false)), Removable),
        (facts(Some(false), None), Removable),
        // A disk image: Foundation doesn't know if it is internal.
        (facts(None, Some(true)), Removable),
        (facts(None, Some(false)), Removable),
        // No facts at all (the CLI, Rust tests): the mount flags decide.
        (facts(None, None), Fixed),
    ] {
        assert_eq!(volume::kind(&info, Some(internal_flags)), kind, "{info:?}");
    }
    let no_facts = facts(None, None);
    let flags = |local, removable| {
        Some(VolumeFlags {
            local,
            removable,
            network_type: false,
        })
    };
    assert_eq!(volume::kind(&no_facts, flags(true, true)), Removable);
    assert_eq!(
        volume::kind(&no_facts, None),
        Removable,
        "unknown flags: assume removable"
    );
    // The facts can't overrule flags that say it is removable.
    assert_eq!(
        volume::kind(&facts(Some(true), Some(false)), flags(true, true)),
        Removable
    );
    // A share is a share, whatever Foundation says.
    for info in [facts(None, None), facts(Some(false), Some(true))] {
        assert_eq!(volume::kind(&info, flags(false, false)), Network);
        assert_eq!(volume::kind(&info, flags(false, true)), Network);
        // A network file system that says it is local is still a share.
        let named = VolumeFlags {
            local: true,
            removable: false,
            network_type: true,
        };
        assert_eq!(volume::kind(&info, Some(named)), Network);
    }
}

/// The network file systems are known by their `f_fstypename`, in any
/// case; local ones aren't (ADR-0009).
#[test]
fn network_file_systems_are_known_by_name() {
    for name in ["smbfs", "nfs", "afpfs", "webdav", "ftp", "SMBFS"] {
        assert!(volume::is_network_file_system(name), "{name}");
    }
    for name in ["apfs", "hfs", "exfat", "msdos", "devfs", "", "smb", "nfs4x"] {
        assert!(!volume::is_network_file_system(name), "{name}");
    }
}

/// A file in a temporary directory is on a local APFS volume: the type
/// name `fstatfs` gives isn't a network one, and the volume is local.
#[test]
fn a_local_volume_is_not_a_share() {
    let dir = TempDir::new("local-flags");
    let path = dir.file("a.csv", b"a\n");
    let flags = volume::flags(&File::open(&path).unwrap()).unwrap();
    assert!(flags.local);
    assert!(!flags.network_type);
    let (_, name) = sys::volume_flags(&File::open(&path).unwrap()).unwrap();
    assert_eq!(name, "apfs");
}

/// A file on the scratch directory's volume (the boot volume) is mapped as
/// before, with no detection at all, even before the scratch directory
/// exists (the clone goes in the app's folder).
#[test]
fn files_on_the_scratch_volume_are_mapped() {
    let dir = TempDir::new("internal-volume");
    let path = dir.file("a.csv", b"a\n");
    let temp = dir.temp_folders();
    let ejectable = VolumeInfo {
        folder: Some(dir.folder("given")),
        is_internal: Some(false),
        is_ejectable: Some(true),
    };
    let source = Source::open_on(&path, &temp, ejectable).unwrap();
    assert_eq!(source.storage(), Storage::Clone);
    assert_eq!(source.as_slice(), Some(&b"a\n"[..]));
    assert!(!temp.scratch().exists());
}

// ---------------------------------------------------------------------------
// Real volumes that vanish (disk images)

/// A disk image is reported as removable by its mount flags, so the
/// detection works without any facts from Foundation; the internal volume
/// isn't.
#[test]
fn a_disk_image_is_detected_as_removable() {
    let image = DiskImage::new("APFS");
    let dir = TempDir::new("detect");
    let on_image = image.root().join("a.csv");
    fs::write(&on_image, b"x,y\n").unwrap();
    let internal = dir.file("b.csv", b"x,y\n");

    let flags = |path: &Path| volume::flags(&File::open(path).unwrap());
    assert!(flags(&on_image).unwrap().removable);
    assert!(!flags(&internal).unwrap().removable);
    assert!(flags(&internal).unwrap().local);

    let given = image.root().join("NSIRD_Leal_test");
    fs::create_dir(&given).unwrap();
    let source = Source::open(&on_image, &dir.temp_folders(), Some(given.clone())).unwrap();
    assert_eq!(source.storage(), Storage::Reading);
    assert_eq!(source.as_slice(), None);
    let clone = source.external_clone().unwrap();
    assert!(clone.starts_with(&given), "cloned on its own volume");
    assert_eq!(device(&clone), device(&on_image));
    assert_eq!(read_all(&source), b"x,y\n");
}

/// The drive vanishes before the copy starts: reads fail with an error
/// instead of crashing, and Save is refused.
#[test]
fn a_drive_disconnected_before_the_copy_fails_reads_without_crashing() {
    let image = DiskImage::new("APFS");
    let dir = TempDir::new("vanish-early");
    let bytes = contents(300_000);
    let path = image.root().join("a.csv");
    fs::write(&path, &bytes).unwrap();
    let given = image.root().join("NSIRD_Leal_test");
    fs::create_dir(&given).unwrap();
    let temp = dir.temp_folders();
    let source = Source::open(&path, &temp, Some(given)).unwrap();
    assert_eq!(
        &*source.read_range(0..64 * 1024).unwrap(),
        &bytes[..64 * 1024],
        "first paint"
    );

    image.force_detach();
    assert_eq!(
        kind(source.read_range(100_000..100_100)),
        ReadErrorKind::Disconnected
    );
    assert_eq!(source.storage(), Storage::Disconnected);
    assert!(!source.can_save());
    assert_eq!(source.available_len(), 0);
    assert_eq!(
        kind(source.read_range(0..10)),
        ReadErrorKind::Disconnected,
        "nothing was copied"
    );
    let streamed = stream_all(&source);
    assert_eq!(kind(streamed.result), ReadErrorKind::Disconnected);
    assert!(streamed.chunks.is_empty());

    drop(source);
    assert!(entries(temp.scratch()).is_empty());
    assert!(entries(temp.records()).is_empty());
}

/// PLAN 1.1a's test: the drive is force-detached part way through the
/// copy. The test process doesn't crash, the source is disconnected, what
/// was copied stays readable, Save is refused, and a new pass delivers the
/// copied part again before reporting the disconnection.
#[test]
fn a_drive_detached_mid_copy_keeps_what_was_copied() {
    let image = DiskImage::new("APFS");
    let dir = TempDir::new("vanish-mid-copy");
    let chunk = 256 * 1024;
    let bytes = contents(16 * chunk);
    let path = image.root().join("a.csv");
    fs::write(&path, &bytes).unwrap();
    let given = image.root().join("NSIRD_Leal_test");
    fs::create_dir(&given).unwrap();
    let temp = dir.temp_folders();
    let source = Source::open_with_options(
        &path,
        &temp,
        VolumeInfo {
            folder: Some(given),
            ..VolumeInfo::default()
        },
        Options {
            chunk_len: chunk,
            ..Options::default()
        },
    )
    .unwrap();
    assert_eq!(source.storage(), Storage::Reading);

    let streamed = stream_with(&source, &AtomicBool::new(false), |n, _| {
        if n == 3 {
            image.force_detach();
        }
    });
    assert_eq!(kind(streamed.result), ReadErrorKind::Disconnected);
    assert_eq!(streamed.chunks, expected_chunks(4 * chunk, chunk));
    assert_eq!(streamed.bytes, &bytes[..4 * chunk]);

    assert_eq!(source.storage(), Storage::Disconnected);
    assert!(!source.can_save());
    assert_eq!(source.available_len(), u64::try_from(4 * chunk).unwrap());
    assert_eq!(source.as_slice(), None);
    // What was copied is still there; anything after it isn't.
    assert_eq!(
        &*source.read_range(0..4 * chunk).unwrap(),
        &bytes[..4 * chunk]
    );
    assert_eq!(
        &*source.read_range(chunk + 5..chunk + 500).unwrap(),
        &bytes[chunk + 5..chunk + 500]
    );
    assert_eq!(
        kind(source.read_range(4 * chunk - 10..4 * chunk + 10)),
        ReadErrorKind::Disconnected
    );

    let again = stream_all(&source);
    assert_eq!(kind(again.result), ReadErrorKind::Disconnected);
    assert_eq!(again.chunks, expected_chunks(4 * chunk, chunk));
    assert_eq!(again.bytes, &bytes[..4 * chunk]);

    drop(source);
    assert!(entries(temp.scratch()).is_empty());
    assert!(entries(temp.records()).is_empty());
}

/// Once the copy is complete, the external clone is deleted and the
/// internal copy is mapped, so the drive vanishing afterwards changes
/// nothing: every byte can still be touched.
#[test]
fn a_completed_copy_survives_the_drive_vanishing() {
    let image = DiskImage::new("APFS");
    let dir = TempDir::new("vanish-after");
    let bytes = contents(3_000_000);
    let path = image.root().join("a.csv");
    fs::write(&path, &bytes).unwrap();
    let given = image.root().join("NSIRD_Leal_test");
    fs::create_dir(&given).unwrap();
    let temp = dir.temp_folders();
    let source = Source::open(&path, &temp, Some(given.clone())).unwrap();
    assert_eq!(source.storage(), Storage::Reading);

    stream_all(&source).result.unwrap();
    assert_eq!(source.storage(), Storage::Copy);
    assert!(!given.exists(), "the external clone's folder is deleted");
    let internal = source.temp.as_ref().unwrap().file_path();
    assert!(internal.starts_with(temp.scratch()));
    assert_ne!(device(&internal), device(&path));

    image.force_detach();
    assert_eq!(source.as_slice(), Some(bytes.as_slice()));
    assert_eq!(read_all(&source), bytes);
    assert_eq!(source.storage(), Storage::Copy);
    assert!(source.can_save());

    drop(source);
    assert!(entries(temp.scratch()).is_empty());
    assert!(entries(temp.records()).is_empty());
}

// ---------------------------------------------------------------------------
// Removable drives that can't clone (exFAT disk images)

/// Opens `path` on an exFAT image, with the folder the app would give and
/// small chunks.
fn open_on_exfat(path: &Path, temp: &TempFolders, given: PathBuf, chunk_len: usize) -> Source {
    Source::open_with_options(
        path,
        temp,
        VolumeInfo {
            folder: Some(given),
            ..VolumeInfo::default()
        },
        Options {
            chunk_len,
            ..Options::default()
        },
    )
    .unwrap()
}

/// ADR-0006 covers every removable drive, including ones that can't clone.
/// Nothing is read at open (first paint doesn't wait for the file), the
/// user's file is read with `pread` and never mapped, and the copy streams
/// in chunks and can be cancelled.
#[test]
fn a_drive_that_cant_clone_is_streamed_not_read_at_open() {
    let image = DiskImage::new("ExFAT");
    let dir = TempDir::new("exfat-stream");
    let chunk = 256 * 1024;
    let bytes = contents(16 * chunk);
    let path = image.root().join("a.csv");
    fs::write(&path, &bytes).unwrap();
    let given = image.root().join("NSIRD_Leal_test");
    fs::create_dir(&given).unwrap();
    let temp = dir.temp_folders();

    // No wall-clock bound here (phase 1 review, tests-11): that nothing is
    // read at open is checked below (still `Reading`, no slice, an empty
    // internal copy), and the time is the gated benchmark
    // `open/first_paint_removable_under_load`'s.
    let source = open_on_exfat(&path, &temp, given.clone(), chunk);
    let first = source.read_range(0..64 * 1024).unwrap();
    assert_eq!(&*first, &bytes[..64 * 1024]);

    assert_eq!(source.storage(), Storage::Reading);
    assert_eq!(source.as_slice(), None);
    assert_eq!(
        source.external_clone(),
        Some(path.clone()),
        "the file itself"
    );
    assert!(!given.exists(), "no clone, so the folder is removed");
    let internal = source.temp.as_ref().unwrap().file_path();
    assert_eq!(
        fs::metadata(&internal).unwrap().len(),
        0,
        "nothing was copied at open"
    );

    let cancel = AtomicBool::new(false);
    let cancelled = stream_with(&source, &cancel, |n, _| {
        if n == 2 {
            cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        }
    });
    assert_eq!(kind(cancelled.result), ReadErrorKind::Cancelled);
    assert_eq!(cancelled.chunks, expected_chunks(3 * chunk, chunk));
    assert_eq!(source.storage(), Storage::Reading);

    let streamed = stream_all(&source);
    streamed.result.unwrap();
    assert_eq!(streamed.chunks, expected_chunks(bytes.len(), chunk));
    assert_eq!(streamed.bytes, bytes);
    assert_eq!(source.storage(), Storage::Copy);
    assert_eq!(source.as_slice(), Some(bytes.as_slice()));
    assert!(!source.changed_on_disk());
    assert_eq!(source.external_clone(), None);

    drop(source);
    assert!(entries(temp.scratch()).is_empty());
    assert!(entries(temp.records()).is_empty());
    assert_eq!(
        fs::read(&path).unwrap(),
        bytes,
        "the user's file is untouched"
    );
}

#[test]
fn a_drive_that_cant_clone_detached_mid_copy_is_disconnected() {
    let image = DiskImage::new("ExFAT");
    let dir = TempDir::new("exfat-vanish");
    let chunk = 256 * 1024;
    let bytes = contents(16 * chunk);
    let path = image.root().join("a.csv");
    fs::write(&path, &bytes).unwrap();
    let given = image.root().join("NSIRD_Leal_test");
    fs::create_dir(&given).unwrap();
    let temp = dir.temp_folders();
    let source = open_on_exfat(&path, &temp, given, chunk);

    let streamed = stream_with(&source, &AtomicBool::new(false), |n, _| {
        if n == 3 {
            image.force_detach();
        }
    });
    assert_eq!(kind(streamed.result), ReadErrorKind::Disconnected);
    assert_eq!(streamed.chunks, expected_chunks(4 * chunk, chunk));
    assert_eq!(source.storage(), Storage::Disconnected);
    assert!(!source.can_save());
    assert_eq!(source.available_len(), u64::try_from(4 * chunk).unwrap());
    assert_eq!(
        &*source.read_range(0..4 * chunk).unwrap(),
        &bytes[..4 * chunk]
    );
    assert_eq!(
        kind(source.read_range(4 * chunk..4 * chunk + 1)),
        ReadErrorKind::Disconnected
    );

    drop(source);
    assert!(entries(temp.scratch()).is_empty());
    assert!(entries(temp.records()).is_empty());
}

/// How a test changes the user's file behind Leal's back.
#[derive(Debug, Clone, Copy)]
enum Change {
    /// Same size, every byte different, a later modification time.
    Rewrite,
    /// Cut short, so the next chunk read comes up short.
    Truncate,
    /// Bytes added at the end.
    Append,
}

impl Change {
    fn apply(self, path: &Path, opened: std::time::SystemTime, len: usize) {
        let file = File::options()
            .write(true)
            .append(matches!(self, Self::Append))
            .open(path)
            .unwrap();
        match self {
            Self::Rewrite => {
                std::os::unix::fs::FileExt::write_all_at(&file, &vec![b'Z'; len], 0).unwrap();
                file.set_modified(opened + Duration::from_secs(10)).unwrap();
            }
            Self::Truncate => file.set_len(u64::try_from(len / 3).unwrap()).unwrap(),
            Self::Append => std::io::Write::write_all(&mut &file, b"extra,row\n").unwrap(),
        }
    }
}

/// Without a clone, another program can change the user's file while it is
/// read. Leal never serves bytes read after the change, never maps the copy,
/// and refuses Save, which would write a mix of two versions over the file.
#[test]
fn changes_to_the_file_without_a_clone_are_reported_and_block_save() {
    let image = DiskImage::new("ExFAT");
    let dir = TempDir::new("exfat-change");
    let chunk = 64 * 1024;
    let bytes = contents(8 * chunk);
    let temp = dir.temp_folders();

    // The reviewer's case: first paint reads the old bytes, then the file
    // is rewritten in place before the copy starts.
    {
        let path = image.root().join("before.csv");
        fs::write(&path, &bytes).unwrap();
        let given = image.root().join("NSIRD_before");
        fs::create_dir(&given).unwrap();
        let source = open_on_exfat(&path, &temp, given, chunk);
        let opened = source.identity().modified.unwrap();
        assert_eq!(&*source.read_range(0..4).unwrap(), &bytes[..4]);
        assert!(source.can_save());

        Change::Rewrite.apply(&path, opened, bytes.len());
        assert_eq!(
            kind(source.read_range(chunk..chunk + 4)),
            ReadErrorKind::ChangedOnDisk,
            "the new bytes are read, then refused"
        );
        assert!(source.changed_on_disk());
        assert!(!source.can_save());
        let streamed = stream_all(&source);
        assert_eq!(kind(streamed.result), ReadErrorKind::ChangedOnDisk);
        assert!(streamed.chunks.is_empty());
        assert_eq!(source.storage(), Storage::Reading);
        assert_eq!(source.as_slice(), None, "never mapped");
    }

    // Changed during the copy, after the second chunk.
    for change in [Change::Rewrite, Change::Truncate, Change::Append] {
        let path = image.root().join(format!("{change:?}.csv"));
        fs::write(&path, &bytes).unwrap();
        let given = image.root().join(format!("NSIRD_{change:?}"));
        fs::create_dir(&given).unwrap();
        let source = open_on_exfat(&path, &temp, given, chunk);
        let opened = source.identity().modified.unwrap();

        let streamed = stream_with(&source, &AtomicBool::new(false), |n, _| {
            if n == 1 {
                change.apply(&path, opened, bytes.len());
            }
        });
        assert_eq!(
            kind(streamed.result),
            ReadErrorKind::ChangedOnDisk,
            "{change:?}"
        );
        // The third chunk was read after the change: not delivered.
        assert_eq!(
            streamed.chunks,
            expected_chunks(2 * chunk, chunk),
            "{change:?}"
        );
        assert_eq!(streamed.bytes, &bytes[..2 * chunk], "{change:?}");
        assert!(source.changed_on_disk(), "{change:?}");
        assert!(!source.can_save(), "{change:?}");
        assert_eq!(source.storage(), Storage::Reading, "{change:?}");
        assert_eq!(source.as_slice(), None, "{change:?}: never mapped");
        // The copied part holds the old bytes; nothing past it is served.
        assert_eq!(
            &*source.read_range(0..2 * chunk).unwrap(),
            &bytes[..2 * chunk],
            "{change:?}"
        );
        assert_eq!(
            kind(source.read_range(2 * chunk..2 * chunk + 1)),
            ReadErrorKind::ChangedOnDisk,
            "{change:?}"
        );
        // It stays reported: a new pass fails straight away.
        let again = stream_all(&source);
        assert_eq!(
            kind(again.result),
            ReadErrorKind::ChangedOnDisk,
            "{change:?}"
        );
        assert!(again.chunks.is_empty(), "{change:?}");
    }

    assert_eq!(entries(temp.scratch()).len(), 0, "every source was dropped");
}

/// Writes `b"XX"` into the user's file at `at`, in place, and puts its
/// modification time back if `keep_time`: a same-size change.
fn change_in_place(path: &Path, at: usize, keep_time: bool) {
    let modified = fs::metadata(path).unwrap().modified().unwrap();
    let file = fs::OpenOptions::new().write(true).open(path).unwrap();
    std::os::unix::fs::FileExt::write_all_at(&file, b"XX", u64::try_from(at).unwrap()).unwrap();
    if keep_time {
        file.set_modified(modified).unwrap();
    }
}

/// Task 2.1a: a drive that comes back without Leal's clone is read from the
/// user's file, though first paint read the clone. First paint's bytes are
/// kept whatever they were read from, so the rest of the first 64 KB is
/// checked against them: a same-size change there, with its modification
/// time put back, passes the `fstat` check but stops the copy.
///
/// With 4 KB chunks; with 5,000-byte chunks, one of which crosses the end
/// of the 64 KB (the change is in its part inside them); and with the
/// app's 1 MB chunks and the drive gone before the first one, so the one
/// chunk holding all of the 64 KB is read from the user's file.
#[test]
fn a_drive_back_without_its_clone_checks_the_copy_against_first_paint() {
    let dir = TempDir::new("reconnect-lost-clone");
    let head = 64 * 1024;
    // (chunk, disconnected at, changed at, file size)
    let cases = [
        (4096, 16 * 1024, 40 * 1024 + 7, 200 * 1024),
        (5000, 16 * 1024, 65_100, 200 * 1024),
        (1024 * 1024, 0, 40 * 1024 + 7, 2_500_000),
    ];
    for (chunk, disconnect_at, changed_at, len) in cases {
        let case = format!("{chunk}-byte chunks");
        let bytes = contents(len);
        let path = dir.file(&format!("usb-{chunk}.csv"), &bytes);
        let source = Source::open_simulating_fault(
            &path,
            &dir.temp_folders(),
            chunk,
            Some(SimulatedFault::Disconnect { at: disconnect_at }),
        )
        .unwrap();
        assert_eq!(&*source.read_head(head).unwrap(), &bytes[..head], "{case}");
        let streamed = stream_all(&source);
        assert_eq!(kind(streamed.result), ReadErrorKind::Disconnected, "{case}");
        let copied = disconnect_at / chunk * chunk;
        assert_eq!(streamed.chunks, expected_chunks(copied, chunk), "{case}");

        assert!(
            source.simulate_clone_lost(),
            "{case}: first paint read a clone"
        );
        change_in_place(&path, changed_at, true);
        source.simulate_drive_back();
        assert!(source.reconnect(&path), "{case}: the file looks unchanged");
        assert!(source.can_save(), "{case}");

        let streamed = stream_all(&source);
        assert_eq!(
            kind(streamed.result),
            ReadErrorKind::ChangedOnDisk,
            "{case}"
        );
        let before_change = changed_at / chunk * chunk;
        assert_eq!(
            streamed.chunks,
            expected_chunks(before_change, chunk),
            "{case}: the changed chunk isn't delivered"
        );
        assert_eq!(streamed.bytes, &bytes[..before_change], "{case}");
        assert!(source.changed_on_disk(), "{case}");
        assert!(!source.can_save(), "{case}");
        assert_eq!(source.as_slice(), None, "{case}: never mapped");
        assert_eq!(source.kept_head_len(), 0, "{case}: let go once they differ");
    }
}

/// Opens `bytes` as `name` on a simulated drive that can clone, reads first
/// paint, and copies until the drive goes at 16 KB; then loses the clone,
/// brings the drive back and reconnects, which falls back to the user's
/// file.
fn fallen_back_to_the_users_file(dir: &TempDir, name: &str, bytes: &[u8]) -> (Source, PathBuf) {
    let path = dir.file(name, bytes);
    let source = Source::open_simulating_fault(
        &path,
        &dir.temp_folders(),
        4096,
        Some(SimulatedFault::Disconnect { at: 16 * 1024 }),
    )
    .unwrap();
    source.read_head(64 * 1024).unwrap();
    assert_eq!(
        kind(stream_all(&source).result),
        ReadErrorKind::Disconnected
    );
    assert!(source.simulate_clone_lost());
    source.simulate_drive_back();
    assert!(source.reconnect(&path));
    assert_eq!(
        source.external_clone(),
        Some(path.clone()),
        "the user's file"
    );
    (source, path)
}

/// The hook deletes only Leal's clone, by its own path: once a reconnect
/// has fallen back to the user's file, there is nothing left to lose, and
/// the user's file is left alone (2.1a review).
#[test]
fn losing_the_clone_again_leaves_the_users_file_alone() {
    let dir = TempDir::new("lose-clone-twice");
    let bytes = contents(200 * 1024);
    let (source, path) = fallen_back_to_the_users_file(&dir, "usb.csv", &bytes);
    assert!(!source.simulate_clone_lost(), "nothing left to lose");
    assert!(!source.simulate_clone_lost(), "still nothing");
    assert_eq!(fs::read(&path).unwrap(), bytes, "the user's file is there");
    let streamed = stream_all(&source);
    streamed.result.unwrap();
    assert_eq!(streamed.bytes, bytes);
}

/// After a reconnect without the clone, the drive's file is the user's,
/// which may simply have been renamed: an `EIO` then is a read error (such
/// as a bad block), not a vanished drive. Only a clone's path is looked at
/// (`drive_vanished`, from 1.9; fixed in the 2.1a review). While the clone
/// is read, a clone gone from its path with an `EIO` is a vanished drive.
#[test]
fn after_falling_back_a_renamed_file_and_an_eio_is_not_a_disconnection() {
    let dir = TempDir::new("fallback-eio");
    let bytes = contents(200 * 1024);
    let (source, path) = fallen_back_to_the_users_file(&dir, "usb.csv", &bytes);
    fs::rename(&path, dir.0.join("renamed.csv")).unwrap();
    assert_eq!(source.classify_eio_now(), Some(ReadErrorKind::Other));
    assert_eq!(source.storage(), Storage::Reading);

    let path = dir.file("cloned.csv", &bytes);
    let source = open_removable(&path, &dir.temp_folders(), 4096);
    assert!(source.simulate_clone_lost());
    assert_eq!(source.classify_eio_now(), Some(ReadErrorKind::Disconnected));
    assert_eq!(source.storage(), Storage::Disconnected);
}

/// The clone a drive comes back with is a snapshot: first paint's bytes,
/// kept although they were read from it, agree with it, and a change to the
/// user's file meanwhile doesn't matter.
#[test]
fn a_drive_back_with_its_clone_completes_the_copy() {
    let dir = TempDir::new("reconnect-kept-clone");
    let chunk = 4096;
    let head = 64 * 1024;
    let bytes = contents(200 * 1024);
    let path = dir.file("usb.csv", &bytes);
    let source = Source::open_simulating_fault(
        &path,
        &dir.temp_folders(),
        chunk,
        Some(SimulatedFault::Disconnect { at: 16 * 1024 }),
    )
    .unwrap();
    assert_eq!(&*source.read_head(head).unwrap(), &bytes[..head]);
    assert_eq!(source.kept_head_len(), head, "kept, though from a clone");
    assert_eq!(
        kind(stream_all(&source).result),
        ReadErrorKind::Disconnected
    );
    assert_eq!(source.kept_head_len(), head, "not all checked yet");

    change_in_place(&path, 40 * 1024, false);
    source.simulate_drive_back();
    assert!(source.reconnect(&path));
    let streamed = stream_all(&source);
    streamed.result.unwrap();
    assert_eq!(streamed.bytes, bytes);
    assert!(!source.changed_on_disk());
    assert_eq!(source.as_slice(), Some(bytes.as_slice()));
    assert_eq!(source.kept_head_len(), 0, "let go once checked");
}

/// First paint's bytes kept from a clone cost at most 64 KB, and only
/// until the copy is past them.
#[test]
fn first_paints_bytes_from_a_clone_are_let_go_once_copied() {
    let dir = TempDir::new("clone-head-kept");
    let bytes = contents(200 * 1024);
    let path = dir.file("usb.csv", &bytes);
    let source = open_removable(&path, &dir.temp_folders(), 4096);
    assert!(source.external_clone().is_some_and(|clone| clone != path));
    source.read_head(64 * 1024).unwrap();
    assert_eq!(source.kept_head_len(), 64 * 1024);
    let mut kept = Vec::new();
    let streamed = stream_with(&source, &AtomicBool::new(false), |_, source| {
        kept.push(source.kept_head_len());
    });
    streamed.result.unwrap();
    assert_eq!(streamed.bytes, bytes);
    // Kept while the copy is in the first 64 KB (16 chunks), then let go.
    assert!(kept[..15].iter().all(|&len| len == 64 * 1024), "{kept:?}");
    assert!(kept[15..].iter().all(|&len| len == 0), "{kept:?}");

    // A file shorter than 64 KB: all of it, until its one chunk is copied.
    let small = dir.file("small.csv", &bytes[..1000]);
    let source = open_removable(&small, &dir.temp_folders(), 4096);
    assert_eq!(&*source.read_head(64 * 1024).unwrap(), &bytes[..1000]);
    assert_eq!(source.kept_head_len(), 1000);
    stream_all(&source).result.unwrap();
    assert_eq!(source.kept_head_len(), 0);
}
