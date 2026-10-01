//! Tests for [`Source`] and [`TempFolders`]. Some mount small disk images
//! (with `hdiutil`, not shown in Finder) to put files on a second APFS
//! volume and on a volume that can't clone (HFS+).

use super::*;
use std::os::unix::ffi::OsStrExt;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use proptest::prelude::*;

/// A temporary directory that is deleted when the test ends.
struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "leal-core-source-{name}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir).unwrap();
        // Canonical, so paths compare equal to what the OS reports
        // (`/var` is a symlink to `/private/var`).
        Self(dir.canonicalize().unwrap())
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn file(&self, name: &str, contents: &[u8]) -> PathBuf {
        let path = self.0.join(name);
        fs::write(&path, contents).unwrap();
        path
    }

    fn folder(&self, name: &str) -> PathBuf {
        let path = self.0.join(name);
        fs::create_dir_all(&path).unwrap();
        path
    }

    /// Leal's temporary locations, inside this directory.
    fn temp_folders(&self) -> TempFolders {
        TempFolders::new(self.0.join("scratch"), self.0.join("records"))
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// A small disk image, attached for the length of a test.
struct DiskImage {
    mount: PathBuf,
    // Dropped after `Drop::drop` has detached the image.
    _dir: TempDir,
}

impl DiskImage {
    /// Creates and attaches a 16 MB image formatted as `fs` (`"APFS"` or
    /// `"HFS+"`), mounted inside a temporary directory, not in `/Volumes`,
    /// and hidden from Finder (`-nobrowse`).
    fn new(fs_type: &str) -> Self {
        let dir = TempDir::new("image");
        let image = dir.path().join("volume.dmg");
        let mount = dir.folder("mnt");
        run(Command::new("hdiutil")
            .args([
                "create", "-quiet", "-size", "16m", "-fs", fs_type, "-volname", "LealTest",
            ])
            .arg(&image));
        run(Command::new("hdiutil")
            .args([
                "attach",
                "-quiet",
                "-nobrowse",
                "-noverify",
                "-noautoopen",
                "-mountpoint",
            ])
            .arg(&mount)
            .arg(&image));
        Self { mount, _dir: dir }
    }

    /// The root of the mounted volume.
    fn root(&self) -> &Path {
        &self.mount
    }
}

impl Drop for DiskImage {
    fn drop(&mut self) {
        let detached = Command::new("hdiutil")
            .args(["detach", "-quiet"])
            .arg(&self.mount)
            .status()
            .is_ok_and(|status| status.success());
        if !detached {
            let _ = Command::new("hdiutil")
                .args(["detach", "-quiet", "-force"])
                .arg(&self.mount)
                .status();
        }
    }
}

fn run(command: &mut Command) {
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{command:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// The volume (`st_dev`) `path` is on.
fn device(path: &Path) -> u64 {
    fs::metadata(path).unwrap().dev()
}

/// Sets an extended attribute with the `xattr` tool. `hex` is the value in
/// hexadecimal, so any bytes can be written.
fn set_attribute(path: &Path, name: &str, hex: &str) {
    run(Command::new("/usr/bin/xattr")
        .arg("-wx")
        .arg(name)
        .arg(hex)
        .arg(path));
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The entries of a directory, or none if it doesn't exist.
fn entries(dir: &Path) -> Vec<PathBuf> {
    match fs::read_dir(dir) {
        Ok(entries) => entries.map(|entry| entry.unwrap().path()).collect(),
        Err(_) => Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// Opening and reading

#[test]
fn source_is_send_and_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Source>();
}

#[test]
fn opens_a_clone_with_the_files_bytes() {
    let dir = TempDir::new("basic");
    let path = dir.file("a.csv", b"name,age\r\nAda,36\r\n");
    let source = Source::open(&path, &dir.temp_folders(), None).unwrap();
    assert_eq!(source.bytes(), b"name,age\r\nAda,36\r\n");
    assert_eq!(source.storage(), Storage::Clone);
    assert_eq!(source.path(), path);
    assert_eq!(source.identity().len, 18);
    assert_eq!(source.identity().inode, fs::metadata(&path).unwrap().ino());
    assert_eq!(source.identity().device, device(&path));
}

#[test]
fn empty_file() {
    let dir = TempDir::new("empty");
    let path = dir.file("empty.csv", b"");
    let source = Source::open(&path, &dir.temp_folders(), None).unwrap();
    assert_eq!(source.bytes(), b"");
    assert_eq!(source.storage(), Storage::Clone);
}

/// The clone is a snapshot: rewriting or truncating the original after
/// opening changes nothing Leal sees, and can't crash it (SIGBUS).
#[test]
fn snapshot_survives_changes_to_the_original() {
    let dir = TempDir::new("snapshot");
    let original = vec![b'x'; 1 << 20];
    let path = dir.file("a.csv", &original);
    let source = Source::open(&path, &dir.temp_folders(), None).unwrap();

    fs::write(&path, b"short").unwrap();
    assert_eq!(source.bytes(), original.as_slice());
    File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_len(0)
        .unwrap();
    assert_eq!(source.bytes(), original.as_slice());
    fs::remove_file(&path).unwrap();
    assert_eq!(source.bytes(), original.as_slice());
}

/// Every corpus file reads back byte for byte.
#[test]
fn corpus_files_read_back_identically() {
    let dir = TempDir::new("corpus");
    let temp = dir.temp_folders();
    let cases = leal_testkit::corpus::load().unwrap();
    assert!(!cases.is_empty());
    for case in cases {
        let source = Source::open(&case.path, &temp, None).unwrap();
        assert_eq!(source.bytes(), case.bytes.as_slice(), "{}", case.name);
        assert_eq!(source.storage(), Storage::Clone, "{}", case.name);
    }
}

proptest! {
    /// Any bytes read back identically (F1 for the reader).
    #[test]
    fn any_bytes_read_back_identically(bytes in leal_testkit::strategies::bytes::csv_bytes()) {
        let dir = TempDir::new("prop");
        let path = dir.file("a.csv", &bytes);
        let source = Source::open(&path, &dir.temp_folders(), None).unwrap();
        prop_assert_eq!(source.bytes(), bytes.as_slice());
    }
}

// ---------------------------------------------------------------------------
// Errors

#[test]
fn missing_file() {
    let dir = TempDir::new("missing");
    let path = dir.path().join("nope.csv");
    let error = Source::open(&path, &dir.temp_folders(), None).unwrap_err();
    assert_eq!(error.kind(), OpenErrorKind::NotFound);
    assert_eq!(error.raw_os_error(), Some(libc::ENOENT));
    assert_eq!(error.path(), path);
}

#[test]
fn missing_folder_in_the_path_is_not_found() {
    let dir = TempDir::new("notdir");
    let file = dir.file("a.csv", b"a");
    let error = Source::open(&file.join("b.csv"), &dir.temp_folders(), None).unwrap_err();
    assert_eq!(error.kind(), OpenErrorKind::NotFound);
    assert_eq!(error.raw_os_error(), Some(libc::ENOTDIR));
}

#[test]
fn permission_denied() {
    let dir = TempDir::new("denied");
    let path = dir.file("locked.csv", b"a,b\n");
    fs::set_permissions(&path, Permissions::from_mode(0o000)).unwrap();
    let error = Source::open(&path, &dir.temp_folders(), None).unwrap_err();
    assert_eq!(error.kind(), OpenErrorKind::PermissionDenied);
    assert_eq!(error.raw_os_error(), Some(libc::EACCES));
    assert_eq!(error.path(), path);
}

#[test]
fn directory() {
    let dir = TempDir::new("directory");
    let folder = dir.folder("folder.csv");
    let error = Source::open(&folder, &dir.temp_folders(), None).unwrap_err();
    assert_eq!(error.kind(), OpenErrorKind::Directory);
    assert_eq!(error.raw_os_error(), Some(libc::EISDIR));
}

#[test]
fn named_pipe_socket_and_device_are_not_files() {
    let dir = TempDir::new("special");
    let fifo = dir.path().join("pipe.csv");
    run(Command::new("mkfifo").arg(&fifo));
    let socket = dir.path().join("socket.csv");
    let _listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    for path in [fifo.as_path(), socket.as_path(), Path::new("/dev/null")] {
        let error = Source::open(path, &dir.temp_folders(), None).unwrap_err();
        assert_eq!(error.kind(), OpenErrorKind::NotAFile, "{}", path.display());
        assert_eq!(error.raw_os_error(), None);
    }
}

/// An error after the file was opened is about Leal's temporary folder,
/// not the user's file, so it isn't worded as "permission denied".
#[test]
fn unwritable_scratch_is_other() {
    let dir = TempDir::new("scratch-denied");
    let path = dir.file("a.csv", b"a,b\n");
    let scratch = dir.folder("scratch");
    fs::set_permissions(&scratch, Permissions::from_mode(0o500)).unwrap();
    let temp = TempFolders::new(&scratch, dir.path().join("records"));
    let error = Source::open(&path, &temp, None).unwrap_err();
    fs::set_permissions(&scratch, Permissions::from_mode(0o700)).unwrap();
    assert_eq!(error.kind(), OpenErrorKind::Other);
    assert_eq!(error.raw_os_error(), Some(libc::EACCES));
    assert_eq!(error.path(), path);
    assert!(
        entries(temp.records()).is_empty(),
        "the failed folder's record is removed"
    );
}

#[test]
fn a_given_folder_is_removed_when_opening_fails() {
    let dir = TempDir::new("given-error");
    let given = dir.folder("given");
    let error = Source::open(
        &dir.path().join("nope.csv"),
        &dir.temp_folders(),
        Some(given.clone()),
    );
    assert_eq!(error.unwrap_err().kind(), OpenErrorKind::NotFound);
    assert!(!given.exists());
}

// ---------------------------------------------------------------------------
// Extended attributes

#[test]
fn reads_raw_attributes() {
    let dir = TempDir::new("attributes");
    let path = dir.file("a.csv", b"a;b\n");
    set_attribute(&path, TEXT_ENCODING_ATTRIBUTE, &hex(b"windows-1252;1280"));
    // Not valid UTF-8: the bytes come through untouched.
    let interpretation = b"\xff{\"delimiter\":\";\"}\x00";
    set_attribute(&path, INTERPRETATION_ATTRIBUTE, &hex(interpretation));
    let source = Source::open(&path, &dir.temp_folders(), None).unwrap();
    assert_eq!(
        source.attributes(),
        &RawAttributes {
            text_encoding: Some(b"windows-1252;1280".to_vec()),
            interpretation: Some(interpretation.to_vec()),
        }
    );
}

#[test]
fn missing_attributes_are_none() {
    let dir = TempDir::new("no-attributes");
    let path = dir.file("a.csv", b"a\n");
    let source = Source::open(&path, &dir.temp_folders(), None).unwrap();
    assert_eq!(source.attributes(), &RawAttributes::default());
}

#[test]
fn an_empty_attribute_is_present_and_empty() {
    let dir = TempDir::new("empty-attribute");
    let path = dir.file("a.csv", b"a\n");
    run(Command::new("/usr/bin/xattr")
        .arg("-w")
        .arg(TEXT_ENCODING_ATTRIBUTE)
        .arg("")
        .arg(&path));
    let source = Source::open(&path, &dir.temp_folders(), None).unwrap();
    assert_eq!(source.attributes().text_encoding, Some(Vec::new()));
}

#[test]
fn an_attribute_over_the_limit_is_none() {
    let dir = TempDir::new("long-attribute");
    let path = dir.file("a.csv", b"a\n");
    set_attribute(
        &path,
        TEXT_ENCODING_ATTRIBUTE,
        &hex(&vec![b'u'; ATTRIBUTE_MAX_BYTES + 1]),
    );
    set_attribute(
        &path,
        INTERPRETATION_ATTRIBUTE,
        &hex(&vec![b'i'; ATTRIBUTE_MAX_BYTES]),
    );
    let source = Source::open(&path, &dir.temp_folders(), None).unwrap();
    assert_eq!(source.attributes().text_encoding, None);
    assert_eq!(
        source.attributes().interpretation.as_ref().map(Vec::len),
        Some(ATTRIBUTE_MAX_BYTES)
    );
}

// ---------------------------------------------------------------------------
// Where the clone goes, and the fallbacks

/// The case ADR-0005 decision 7 is about: a file on a second APFS volume is
/// cloned into the folder on its own volume, not copied.
#[test]
fn file_on_a_second_apfs_volume_is_cloned_on_that_volume() {
    let image = DiskImage::new("APFS");
    let dir = TempDir::new("second-volume");
    let path = image.root().join("a.csv");
    fs::write(&path, b"x,y\n1,2\n").unwrap();
    let given = image.root().join("NSIRD_Leal_test");
    fs::create_dir(&given).unwrap();
    assert_ne!(
        device(&path),
        device(dir.path()),
        "the image is a different volume"
    );

    let source = Source::open(&path, &dir.temp_folders(), Some(given.clone())).unwrap();
    assert_eq!(source.storage(), Storage::Clone);
    assert_eq!(source.bytes(), b"x,y\n1,2\n");
    let clone = source.temp.as_ref().unwrap().file_path();
    assert!(clone.starts_with(&given));
    assert_eq!(device(&clone), device(&path));

    drop(source);
    assert!(
        !given.exists(),
        "the given folder is removed with the source"
    );
    assert!(entries(dir.path().join("records").as_path()).is_empty());
}

/// Without a folder on its volume, a file on another APFS volume gets
/// `EXDEV` from the scratch directory and falls back to memory.
#[test]
fn file_on_another_volume_without_a_folder_there_falls_back() {
    let image = DiskImage::new("APFS");
    let dir = TempDir::new("exdev");
    let path = image.root().join("a.csv");
    fs::write(&path, b"x\n").unwrap();
    let source = Source::open(&path, &dir.temp_folders(), None).unwrap();
    assert_eq!(source.storage(), Storage::Memory);
    assert_eq!(source.bytes(), b"x\n");
}

/// EXDEV means "clone elsewhere": a given folder that turns out to be on
/// another volume is dropped, and the clone goes in the scratch directory,
/// which is on the file's volume.
#[test]
fn given_folder_on_the_wrong_volume_clones_elsewhere() {
    let image = DiskImage::new("APFS");
    let dir = TempDir::new("wrong-volume");
    let path = dir.file("a.csv", b"x\n");
    let given = image.root().join("NSIRD_Leal_test");
    fs::create_dir(&given).unwrap();

    let source = Source::open(&path, &dir.temp_folders(), Some(given.clone())).unwrap();
    assert_eq!(source.storage(), Storage::Clone);
    assert!(!given.exists());
    let clone = source.temp.as_ref().unwrap().file_path();
    assert!(clone.starts_with(dir.path().join("scratch")));
}

/// A volume that can't clone at all (HFS+) gets `ENOTSUP`: the file is read
/// into memory.
#[test]
fn volume_that_cant_clone_reads_into_memory() {
    let image = DiskImage::new("HFS+");
    let dir = TempDir::new("hfs");
    let path = image.root().join("a.csv");
    fs::write(&path, b"a,b\n1,2\n").unwrap();
    let given = image.root().join("NSIRD_Leal_test");
    fs::create_dir(&given).unwrap();

    let source = Source::open(&path, &dir.temp_folders(), Some(given.clone())).unwrap();
    assert_eq!(source.storage(), Storage::Memory);
    assert_eq!(source.bytes(), b"a,b\n1,2\n");
    assert!(!given.exists());
    assert!(entries(&dir.path().join("records")).is_empty());

    // Also a snapshot.
    fs::write(&path, b"").unwrap();
    assert_eq!(source.bytes(), b"a,b\n1,2\n");
}

/// Above the memory limit, a file on a volume that can't clone is copied to
/// the scratch directory and the copy is mapped.
#[test]
fn large_file_on_a_volume_that_cant_clone_is_copied() {
    let image = DiskImage::new("HFS+");
    let dir = TempDir::new("copy");
    let contents: Vec<u8> = (0..3_000_000_u32)
        .map(|i| u8::try_from(i % 251).unwrap())
        .collect();
    let path = image.root().join("big.csv");
    fs::write(&path, &contents).unwrap();
    let temp = dir.temp_folders();

    let source = Source::open_with_memory_limit(&path, &temp, None, 1_000_000).unwrap();
    assert_eq!(source.storage(), Storage::Copy);
    assert_eq!(source.bytes(), contents.as_slice());
    let copy = source.temp.as_ref().unwrap().file_path();
    assert!(copy.starts_with(temp.scratch()));
    assert_eq!(
        fs::metadata(&copy).unwrap().permissions().mode() & 0o777,
        0o400
    );

    fs::write(&path, b"").unwrap();
    assert_eq!(source.bytes(), contents.as_slice());
    drop(source);
    assert!(entries(temp.scratch()).is_empty());
    assert!(entries(temp.records()).is_empty());
}

#[test]
fn file_exactly_at_the_memory_limit_is_read_into_memory() {
    let image = DiskImage::new("HFS+");
    let dir = TempDir::new("limit");
    let path = image.root().join("a.csv");
    fs::write(&path, b"12345").unwrap();
    let temp = dir.temp_folders();
    let at_limit = Source::open_with_memory_limit(&path, &temp, None, 5).unwrap();
    assert_eq!(at_limit.storage(), Storage::Memory);
    let over_limit = Source::open_with_memory_limit(&path, &temp, None, 4).unwrap();
    assert_eq!(over_limit.storage(), Storage::Copy);
    assert_eq!(over_limit.bytes(), b"12345");
}

// ---------------------------------------------------------------------------
// Temporary folders and cleanup

#[test]
fn the_clone_is_read_only_and_in_a_recorded_folder() {
    let dir = TempDir::new("recorded");
    let path = dir.file("a.csv", b"a\n");
    fs::set_permissions(&path, Permissions::from_mode(0o666)).unwrap();
    let temp = dir.temp_folders();
    let source = Source::open(&path, &temp, None).unwrap();
    let folder = source.temp.as_ref().unwrap();
    let clone = folder.file_path();
    assert_eq!(
        fs::metadata(&clone).unwrap().permissions().mode() & 0o777,
        0o400
    );
    assert_eq!(
        fs::metadata(folder.folder()).unwrap().permissions().mode() & 0o777,
        0o700
    );
    let record = fs::read(folder.record_path()).unwrap();
    assert_eq!(record, folder.folder().as_os_str().as_bytes());
    assert_eq!(
        entries(temp.records()),
        vec![folder.record_path().to_owned()]
    );
}

#[test]
fn dropping_the_source_removes_its_clone_folder_and_record() {
    let dir = TempDir::new("drop");
    let path = dir.file("a.csv", b"a\n");
    let temp = dir.temp_folders();
    let source = Source::open(&path, &temp, None).unwrap();
    assert_eq!(entries(temp.scratch()).len(), 1);
    assert_eq!(entries(temp.records()).len(), 1);
    drop(source);
    assert!(entries(temp.scratch()).is_empty());
    assert!(entries(temp.records()).is_empty());
    assert!(path.exists(), "the user's file is untouched");
}

#[test]
fn memory_fallback_leaves_no_temporary_folder() {
    let image = DiskImage::new("HFS+");
    let dir = TempDir::new("memory-clean");
    let path = image.root().join("a.csv");
    fs::write(&path, b"a\n").unwrap();
    let temp = dir.temp_folders();
    let source = Source::open(&path, &temp, None).unwrap();
    assert_eq!(source.storage(), Storage::Memory);
    assert!(entries(temp.scratch()).is_empty());
    assert!(entries(temp.records()).is_empty());
}

/// After a crash, the next launch removes the abandoned clone, its folder
/// and its record.
#[test]
fn cleanup_removes_what_a_crash_left() {
    let dir = TempDir::new("crash");
    let path = dir.file("a.csv", b"a\n");
    let temp = dir.temp_folders();
    let source = Source::open(&path, &temp, None).unwrap();
    let folder = source.temp.as_ref().unwrap().folder().to_owned();
    crash(source);
    assert!(folder.exists());

    assert_eq!(temp.remove_leftovers().unwrap(), 1);
    assert!(!folder.exists());
    assert!(entries(temp.scratch()).is_empty());
    assert!(entries(temp.records()).is_empty());
    assert!(path.exists());
}

/// Cleanup checks every recorded folder, including one on another volume.
#[test]
fn cleanup_removes_a_crashed_clone_on_another_volume() {
    let image = DiskImage::new("APFS");
    let dir = TempDir::new("crash-volume");
    let path = image.root().join("a.csv");
    fs::write(&path, b"a\n").unwrap();
    let given = image.root().join("NSIRD_Leal_test");
    fs::create_dir(&given).unwrap();
    let temp = dir.temp_folders();
    crash(Source::open(&path, &temp, Some(given.clone())).unwrap());
    assert!(given.exists());

    assert_eq!(temp.remove_leftovers().unwrap(), 1);
    assert!(!given.exists());
    assert!(entries(temp.records()).is_empty());
}

/// A folder another running Leal (here, a live source in this process) is
/// using is left alone.
#[test]
fn cleanup_leaves_folders_in_use() {
    let dir = TempDir::new("in-use");
    let path = dir.file("a.csv", b"a\n");
    let temp = dir.temp_folders();
    let source = Source::open(&path, &temp, None).unwrap();
    assert_eq!(temp.remove_leftovers().unwrap(), 0);
    assert_eq!(source.bytes(), b"a\n");
    assert!(source.temp.as_ref().unwrap().file_path().exists());
    assert_eq!(entries(temp.records()).len(), 1);
}

#[test]
fn cleanup_with_no_records_directory() {
    let dir = TempDir::new("no-records");
    assert_eq!(dir.temp_folders().remove_leftovers().unwrap(), 0);
}

/// Cleanup deletes only Leal's own file: anything else in a recorded folder
/// stays, and so does the folder and its record.
#[test]
fn cleanup_never_deletes_other_files() {
    let dir = TempDir::new("other-files");
    let path = dir.file("a.csv", b"a\n");
    let temp = dir.temp_folders();
    let source = Source::open(&path, &temp, None).unwrap();
    let folder = source.temp.as_ref().unwrap().folder().to_owned();
    let clone = source.temp.as_ref().unwrap().file_path();
    crash(source);
    let other = folder.join("someone-elses.txt");
    fs::write(&other, b"keep me").unwrap();

    assert_eq!(temp.remove_leftovers().unwrap(), 0);
    assert!(!clone.exists());
    assert_eq!(fs::read(&other).unwrap(), b"keep me");
    assert_eq!(entries(temp.records()).len(), 1, "kept, to retry later");
}

/// A record of a folder on a volume that isn't mounted now is kept for a
/// later launch; a record of a folder that is simply gone is deleted.
#[test]
fn cleanup_keeps_records_of_unmounted_volumes() {
    let dir = TempDir::new("unmounted");
    let temp = dir.temp_folders();
    let records = temp.records().to_owned();
    fs::create_dir_all(&records).unwrap();
    let unmounted = format!(
        "/Volumes/leal-test-not-mounted-{}/.TemporaryItems/NSIRD_Leal_x",
        std::process::id()
    );
    fs::write(records.join("1-1-1.record"), &unmounted).unwrap();
    let gone = dir.path().join("scratch/leal-2-2-2");
    fs::write(records.join("2-2-2.record"), gone.as_os_str().as_bytes()).unwrap();

    assert_eq!(temp.remove_leftovers().unwrap(), 0);
    assert_eq!(entries(&records), vec![records.join("1-1-1.record")]);
}

/// Hands a source's temporary folder over as a crash would: its record's
/// lock is released, and nothing is deleted.
fn crash(mut source: Source) {
    source.temp.take().unwrap().abandon();
}
