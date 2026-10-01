//! Tests for [`Source`] and [`TempFolders`]. Some mount small disk images
//! (with `hdiutil`, not shown in Finder) to put files on a second APFS
//! volume and on a volume that can't clone (HFS+).

use super::*;
use std::os::unix::ffi::OsStrExt;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use proptest::prelude::*;

mod original;
mod removable;

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
        if fs::remove_dir_all(&self.0).is_err() {
            // A failed test may leave a locked (`uchg`) file behind.
            let _ = Command::new("/usr/bin/chflags")
                .arg("-R")
                .arg("0")
                .arg(&self.0)
                .status();
            let _ = fs::remove_dir_all(&self.0);
        }
    }
}

/// A small disk image, attached for the length of a test.
///
/// Tests that use one are in the `disk-images` test group in
/// `.config/nextest.toml`, which runs them one at a time: `hdiutil`
/// sometimes fails with "Resource busy" when several images are created or
/// attached at once. [`DiskImage::new`] checks that its test is in the group.
pub(crate) struct DiskImage {
    image: PathBuf,
    mount: PathBuf,
    /// Another empty folder to mount it at, as another Mac would.
    elsewhere: PathBuf,
    // Dropped after `Drop::drop` has detached the image.
    _dir: TempDir,
}

/// The nextest test group that runs the disk-image tests one at a time.
const DISK_IMAGE_TEST_GROUP: &str = "disk-images";

/// How many times `hdiutil create` and `attach` are tried before a test
/// fails, and the wait before the first retry (doubled for each one after).
const HDIUTIL_ATTEMPTS: u32 = 5;
const HDIUTIL_FIRST_BACKOFF: Duration = Duration::from_millis(250);

/// Errors `hdiutil` reports when another image is being created, attached
/// or detached at the same moment, or when the disk arbitration daemon is
/// still catching up. Trying again shortly afterwards usually works.
const TRANSIENT_HDIUTIL_ERRORS: [&str; 4] = [
    "Resource busy",
    "Resource temporarily unavailable",
    "no mountable file systems",
    "Device not configured",
];

impl DiskImage {
    /// Creates and attaches a 16 MB image formatted as `fs` (`"APFS"`, `"ExFAT"` or
    /// `"HFS+"`), mounted inside a temporary directory, not in `/Volumes`,
    /// and hidden from Finder (`-nobrowse`).
    pub(crate) fn new(fs_type: &str) -> Self {
        // Under nextest, a disk-image test outside the group would run in
        // parallel with the others.
        if std::env::var_os("NEXTEST").is_some() {
            assert_eq!(
                std::env::var("NEXTEST_TEST_GROUP").as_deref(),
                Ok(DISK_IMAGE_TEST_GROUP),
                "add this test to the {DISK_IMAGE_TEST_GROUP} test group in .config/nextest.toml"
            );
        }

        // The directory's name is unique to this process and image, and so
        // is the volume name, so images made at the same time never meet.
        let dir = TempDir::new("image");
        let image = dir.path().join("volume.dmg");
        let mount = dir.folder("mnt");
        let elsewhere = dir.folder("elsewhere");
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        // At most 11 characters, the limit for an exFAT label: a process ID
        // is at most 5 hex digits.
        let volume_name = format!(
            "L{:x}-{:x}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        );

        hdiutil_with_retries("create", || {
            // A failed attempt may leave a partial image behind.
            let _ = fs::remove_file(&image);
            let mut command = Command::new("/usr/bin/hdiutil");
            command
                .args(["create", "-size", "16m", "-fs", fs_type, "-volname"])
                .arg(&volume_name)
                .arg(&image);
            command
        });
        // From here on, `Drop` detaches the image, even if attaching fails
        // part way (attached, but not mounted).
        let disk_image = Self {
            image,
            mount,
            elsewhere,
            _dir: dir,
        };
        disk_image.attach();
        disk_image
    }

    /// The root of the mounted volume.
    pub(crate) fn root(&self) -> &Path {
        &self.mount
    }

    /// Attaches the image (again) at its usual mount point, [`root`](Self::root),
    /// as when a drive is plugged back in.
    pub(crate) fn attach(&self) {
        self.attach_at(&self.mount);
    }

    /// Attaches the image at another mount point, as another Mac would see
    /// it, and returns that point.
    pub(crate) fn attach_elsewhere(&self) -> &Path {
        self.attach_at(&self.elsewhere);
        &self.elsewhere
    }

    fn attach_at(&self, mount: &Path) {
        hdiutil_with_retries("attach", || {
            self.detach_all();
            // A forced detach can leave the mount point removed.
            let _ = fs::create_dir_all(mount);
            let mut command = Command::new("/usr/bin/hdiutil");
            command
                .args(["attach", "-nobrowse", "-noverify", "-noautoopen"])
                .arg("-mountpoint")
                .arg(mount)
                .arg(&self.image);
            command
        });
    }

    /// Detaches every device attached from this image, trying a normal
    /// detach first and then `-force`. Returns the devices still attached.
    pub(crate) fn detach_all(&self) -> Vec<String> {
        for force in [false, false, true, true] {
            let devices = attached_devices(&self.image);
            if devices.is_empty() {
                return devices;
            }
            for device in &devices {
                let mut command = Command::new("/usr/bin/hdiutil");
                command.arg("detach").arg(device);
                if force {
                    command.arg("-force");
                }
                // A failure here shows up as a device still attached.
                let _ = command.output();
            }
            std::thread::sleep(HDIUTIL_FIRST_BACKOFF);
        }
        attached_devices(&self.image)
    }

    /// Detaches the image with `-force` straight away, as if the drive were
    /// unplugged with files on it still open (a normal detach is refused
    /// then). Panics if the image is still attached afterwards.
    pub(crate) fn force_detach(&self) {
        let mut backoff = HDIUTIL_FIRST_BACKOFF;
        for _ in 0..HDIUTIL_ATTEMPTS {
            let devices = attached_devices(&self.image);
            if devices.is_empty() {
                return;
            }
            for device in &devices {
                // A failure here shows up as a device still attached.
                let _ = Command::new("/usr/bin/hdiutil")
                    .args(["detach", "-force"])
                    .arg(device)
                    .output();
            }
            if attached_devices(&self.image).is_empty() {
                return;
            }
            std::thread::sleep(backoff);
            backoff *= 2;
        }
        let left = attached_devices(&self.image);
        assert!(
            left.is_empty(),
            "couldn't force-detach {} ({})",
            self.image.display(),
            left.join(", ")
        );
    }
}

impl Drop for DiskImage {
    fn drop(&mut self) {
        let left = self.detach_all();
        if !left.is_empty() {
            // Not a panic: this may run while the test is already panicking,
            // and a second panic would abort without the first one's message.
            eprintln!(
                "warning: couldn't detach {} ({}); run `hdiutil detach -force` on it",
                self.image.display(),
                left.join(", ")
            );
        }
    }
}

/// The whole-disk devices (`/dev/diskN`) attached from `image`, from
/// `hdiutil info`. Empty if none are, or if `hdiutil info` fails.
fn attached_devices(image: &Path) -> Vec<String> {
    let Ok(output) = Command::new("/usr/bin/hdiutil").arg("info").output() else {
        return Vec::new();
    };
    let info = String::from_utf8_lossy(&output.stdout);
    let image = image.to_string_lossy();
    // One section per attached image, each starting with a line of `=`.
    info.split("\n=")
        .filter(|section| {
            section.lines().any(|line| {
                line.split_once(':')
                    .is_some_and(|(key, value)| key.trim() == "image-path" && value.trim() == image)
            })
        })
        // The first device listed is the whole disk; detaching it detaches
        // its partitions and any APFS container on it.
        .filter_map(|section| {
            section
                .lines()
                .find(|line| line.starts_with("/dev/disk"))
                .and_then(|line| line.split_whitespace().next())
                .map(str::to_owned)
        })
        .collect()
}

/// Runs the `hdiutil` command `make_command` builds (a new one for each
/// attempt), retrying with a backoff while it fails with an error in
/// [`TRANSIENT_HDIUTIL_ERRORS`]. Panics with the last attempt's exit status,
/// stdout and stderr if it never succeeds.
fn hdiutil_with_retries(what: &str, mut make_command: impl FnMut() -> Command) {
    let mut backoff = HDIUTIL_FIRST_BACKOFF;
    for attempt in 1..=HDIUTIL_ATTEMPTS {
        let mut command = make_command();
        let output = command.output().unwrap();
        if output.status.success() {
            return;
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let transient = TRANSIENT_HDIUTIL_ERRORS
            .iter()
            .any(|error| stdout.contains(error) || stderr.contains(error));
        if !transient || attempt == HDIUTIL_ATTEMPTS {
            panic!(
                "hdiutil {what} failed on attempt {attempt} of {HDIUTIL_ATTEMPTS} \
                 ({}): {command:?}\n{}",
                if transient {
                    "transient error"
                } else {
                    "not a transient error, so not retried"
                },
                describe(&output)
            );
        }
        eprintln!(
            "hdiutil {what} failed on attempt {attempt} of {HDIUTIL_ATTEMPTS}, \
             retrying in {backoff:?}\n{}",
            describe(&output)
        );
        std::thread::sleep(backoff);
        backoff *= 2;
    }
}

/// A command's exit status, stdout and stderr, for a failure message.
fn describe(output: &std::process::Output) -> String {
    format!(
        "{}\n--- stdout ---\n{}\n--- stderr ---\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout).trim_end(),
        String::from_utf8_lossy(&output.stderr).trim_end()
    )
}

fn run(command: &mut Command) {
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{command:?} failed: {}",
        describe(&output)
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

/// Opens `path` as if its volume were internal (a disk image standing in
/// for a second internal disk or partition), with the given folder and
/// memory limit.
fn open_as_internal(
    path: &Path,
    temp: &TempFolders,
    folder: Option<PathBuf>,
    memory_limit: u64,
) -> Source {
    Source::open_with_options(
        path,
        temp,
        VolumeInfo {
            folder,
            ..VolumeInfo::default()
        },
        Options {
            memory_limit,
            volume: VolumeCheck::Internal,
            ..Options::default()
        },
    )
    .unwrap()
}

/// A mapped or in-memory source's bytes, which `read_range` must also
/// give, borrowed.
fn bytes_of(source: &Source) -> &[u8] {
    let slice = source
        .as_slice()
        .expect("a source on an internal volume has one slice");
    let read = source.read_range(0..slice.len()).unwrap();
    assert!(matches!(read, std::borrow::Cow::Borrowed(_)));
    assert_eq!(&*read, slice);
    slice
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
    assert_eq!(bytes_of(&source), b"name,age\r\nAda,36\r\n");
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
    assert_eq!(bytes_of(&source), b"");
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
    assert_eq!(bytes_of(&source), original.as_slice());
    File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_len(0)
        .unwrap();
    assert_eq!(bytes_of(&source), original.as_slice());
    fs::remove_file(&path).unwrap();
    assert_eq!(bytes_of(&source), original.as_slice());
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
        assert_eq!(bytes_of(&source), case.bytes.as_slice(), "{}", case.name);
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
        prop_assert_eq!(bytes_of(&source), bytes.as_slice());
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
/// cloned into the folder on its own volume, not copied. A disk image is
/// removable, so the test treats it as internal (a second internal disk)
/// to reach the mapped clone; `removable::a_disk_image_is_detected_as_removable`
/// covers the image as it is.
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

    let source = Source::open_with_options(
        &path,
        &dir.temp_folders(),
        VolumeInfo {
            folder: Some(given.clone()),
            ..VolumeInfo::default()
        },
        Options {
            volume: VolumeCheck::Internal,
            ..Options::default()
        },
    )
    .unwrap();
    assert_eq!(source.storage(), Storage::Clone);
    assert_eq!(bytes_of(&source), b"x,y\n1,2\n");
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
/// `EXDEV` from the scratch directory and falls back: to memory on an
/// internal volume, and to reading the file itself on a removable one.
#[test]
fn file_on_another_volume_without_a_folder_there_falls_back() {
    let image = DiskImage::new("APFS");
    let dir = TempDir::new("exdev");
    let path = image.root().join("a.csv");
    fs::write(&path, b"x\n").unwrap();
    let source = open_as_internal(&path, &dir.temp_folders(), None, MEMORY_FALLBACK_MAX_BYTES);
    assert_eq!(source.storage(), Storage::Memory);
    assert_eq!(bytes_of(&source), b"x\n");

    let removable = Source::open(&path, &dir.temp_folders(), None).unwrap();
    assert_eq!(removable.storage(), Storage::Reading);
    assert_eq!(
        removable.external_clone(),
        Some(path.clone()),
        "reads the file itself"
    );
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

/// An internal volume that can't clone at all (HFS+; the image stands in
/// for an internal partition) gets `ENOTSUP`: the file is read into memory.
#[test]
fn volume_that_cant_clone_reads_into_memory() {
    let image = DiskImage::new("HFS+");
    let dir = TempDir::new("hfs");
    let path = image.root().join("a.csv");
    fs::write(&path, b"a,b\n1,2\n").unwrap();
    let given = image.root().join("NSIRD_Leal_test");
    fs::create_dir(&given).unwrap();

    let source = open_as_internal(
        &path,
        &dir.temp_folders(),
        Some(given.clone()),
        MEMORY_FALLBACK_MAX_BYTES,
    );
    assert_eq!(source.storage(), Storage::Memory);
    assert_eq!(bytes_of(&source), b"a,b\n1,2\n");
    assert!(!given.exists());
    assert!(entries(&dir.path().join("records")).is_empty());

    // Also a snapshot.
    fs::write(&path, b"").unwrap();
    assert_eq!(bytes_of(&source), b"a,b\n1,2\n");
}

/// Above the memory limit, a file on an internal volume that can't clone is
/// copied to the scratch directory and the copy is mapped.
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

    let source = open_as_internal(&path, &temp, None, 1_000_000);
    assert_eq!(source.storage(), Storage::Copy);
    assert_eq!(bytes_of(&source), contents.as_slice());
    let copy = source.temp.as_ref().unwrap().file_path();
    assert!(copy.starts_with(temp.scratch()));
    assert_eq!(
        fs::metadata(&copy).unwrap().permissions().mode() & 0o777,
        0o400
    );

    fs::write(&path, b"").unwrap();
    assert_eq!(bytes_of(&source), contents.as_slice());
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
    let at_limit = open_as_internal(&path, &temp, None, 5);
    assert_eq!(at_limit.storage(), Storage::Memory);
    let over_limit = open_as_internal(&path, &temp, None, 4);
    assert_eq!(over_limit.storage(), Storage::Copy);
    assert_eq!(bytes_of(&over_limit), b"12345");
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
    let source = open_as_internal(&path, &temp, None, MEMORY_FALLBACK_MAX_BYTES);
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
    // The image is removable, so there are two folders: the clone on the
    // image and the internal copy in the scratch directory.
    let source = Source::open(&path, &temp, Some(given.clone())).unwrap();
    assert_eq!(source.storage(), Storage::Reading);
    crash(source);
    assert!(given.exists());
    assert_eq!(entries(temp.scratch()).len(), 1);

    assert_eq!(temp.remove_leftovers().unwrap(), 2);
    assert!(!given.exists());
    assert!(entries(temp.scratch()).is_empty());
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
    assert_eq!(bytes_of(&source), b"a\n");
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

/// Hands a source's temporary folders over as a crash would: their
/// records' locks are released, and nothing is deleted.
fn crash(mut source: Source) {
    source.temp.take().unwrap().abandon();
    source.abandon_external_clone();
}

// ---------------------------------------------------------------------------
// Locked and append-only originals

/// Sets BSD file flags with the `chflags` tool (`uchg`, `uappnd`, or `0`).
/// A user may set and clear the `u…` flags on their own files.
fn chflags(flags: &str, path: &Path) {
    run(Command::new("/usr/bin/chflags").arg(flags).arg(path));
}

fn file_flags(path: &Path) -> u32 {
    use std::os::macos::fs::MetadataExt as _;
    fs::symlink_metadata(path).unwrap().st_flags()
}

/// Clears an original's flags when the test ends, so its `TempDir` can be
/// deleted.
struct Unlock(PathBuf);

impl Drop for Unlock {
    fn drop(&mut self) {
        let _ = Command::new("/usr/bin/chflags")
            .arg("0")
            .arg(&self.0)
            .status();
    }
}

/// `fclonefileat` copies the original's flags. A Finder-locked (`uchg`) or
/// append-only (`uappnd`) original must still give a read-only clone that
/// is deleted with the source, not one that leaks on every open.
#[test]
fn locked_and_append_only_originals_give_a_deletable_read_only_clone() {
    for flag in ["uchg", "uappnd"] {
        let dir = TempDir::new(flag);
        let path = dir.file("a.csv", b"a,b\n1,2\n");
        chflags(flag, &path);
        let _unlock = Unlock(path.clone());
        assert_ne!(
            file_flags(&path) & (libc::UF_IMMUTABLE | libc::UF_APPEND),
            0
        );
        let temp = dir.temp_folders();

        let source = Source::open(&path, &temp, None).unwrap();
        assert_eq!(source.storage(), Storage::Clone, "{flag}");
        assert_eq!(bytes_of(&source), b"a,b\n1,2\n", "{flag}");
        let clone = source.temp.as_ref().unwrap().file_path();
        assert_eq!(
            file_flags(&clone) & (libc::UF_IMMUTABLE | libc::UF_APPEND),
            0,
            "{flag}"
        );
        assert_eq!(
            fs::metadata(&clone).unwrap().permissions().mode() & 0o777,
            0o400,
            "{flag}"
        );

        drop(source);
        assert!(
            entries(temp.scratch()).is_empty(),
            "{flag}: the clone leaked"
        );
        assert!(entries(temp.records()).is_empty(), "{flag}");
        assert_ne!(
            file_flags(&path) & (libc::UF_IMMUTABLE | libc::UF_APPEND),
            0,
            "{flag}: the original keeps its flag"
        );
    }
}

/// A clone left locked by a crash (before its flags were cleared) is still
/// removed by cleanup.
#[test]
fn cleanup_removes_a_locked_leftover_clone() {
    let dir = TempDir::new("locked-leftover");
    let path = dir.file("a.csv", b"a\n");
    let temp = dir.temp_folders();
    let source = Source::open(&path, &temp, None).unwrap();
    let clone = source.temp.as_ref().unwrap().file_path();
    crash(source);
    chflags("uchg", &clone);
    let _unlock = Unlock(clone.clone());

    assert_eq!(temp.remove_leftovers().unwrap(), 1);
    assert!(entries(temp.scratch()).is_empty());
    assert!(entries(temp.records()).is_empty());
}
