//! Tests for [`Source`] and [`TempFolders`]. Some mount small disk images
//! (with `hdiutil`, not shown in Finder) to put files on a second APFS
//! volume and on a volume that can't clone (HFS+).

use super::*;
use std::os::unix::ffi::OsStrExt;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use proptest::prelude::*;

mod original;
mod removable;
mod share;

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

/// Sets (or, with `None`, removes) the extended attribute `name` of the file
/// at `path` (other modules' tests: an attribute the file had when opened).
pub(crate) fn write_attribute(path: &Path, name: &CStr, value: Option<&[u8]>) {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .unwrap();
    match value {
        Some(value) => sys::set_xattr(&file, name, value).unwrap(),
        None => sys::remove_xattr(&file, name).unwrap(),
    }
}

/// Tries to set the extended attribute `name` of the file at `path`:
/// whether it could (the system protects some from apps).
pub(crate) fn try_write_attribute(path: &Path, name: &CStr, value: &[u8]) -> bool {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .unwrap();
    sys::set_xattr(&file, name, value).is_ok()
}

/// The extended attribute `name` of the file at `path`, if it has it.
pub(crate) fn attribute(path: &Path, name: &CStr) -> Option<Vec<u8>> {
    let file = File::open(path).unwrap();
    sys::read_whole_xattr(&file, name).unwrap()
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
    /// `"HFS+"`; 40 MB for `"MS-DOS FAT32"`, the least FAT32 allows),
    /// mounted inside a temporary directory, not in `/Volumes`, and hidden
    /// from Finder (`-nobrowse`).
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

        let size = if fs_type == "MS-DOS FAT32" {
            "40m"
        } else {
            "16m"
        };
        hdiutil_with_retries("create", || {
            // A failed attempt may leave a partial image behind.
            let _ = fs::remove_file(&image);
            let mut command = Command::new("/usr/bin/hdiutil");
            command
                .args(["create", "-size", size, "-fs", fs_type, "-volname"])
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

    /// Detaches every device attached from this image, as an eject does:
    /// an ordinary detach, tried again with a backoff for
    /// [`DETACH_NORMALLY_FOR`] while the volume is busy, then `-force` for
    /// [`DETACH_FORCED_FOR`]. Returns the devices still attached after that:
    /// a leaked image. What `hdiutil` said along the way goes to stderr,
    /// which nextest shows when the test fails.
    pub(crate) fn detach_all(&self) -> Vec<String> {
        self.detach(DETACH_NORMALLY_FOR, DETACH_FORCED_FOR)
    }

    /// Detaches the image with `-force` straight away, as if the drive were
    /// unplugged with files on it still open (a normal detach is refused
    /// then). Panics if the image is still attached, or its volume still
    /// mounted, afterwards: a test that goes on to expect the drive gone
    /// would otherwise fail in a way that looks like Leal's fault.
    pub(crate) fn force_detach(&self) {
        let left = self.detach(Duration::ZERO, DETACH_FORCED_FOR);
        assert!(
            left.is_empty(),
            "couldn't force-detach {} ({}); hdiutil's output is on stderr",
            self.image.display(),
            left.join(", ")
        );
    }

    /// Detaches the image's devices: without `-force` for `normally_for`,
    /// then with it until `forced_for` more has passed. Returns the devices
    /// still attached.
    ///
    /// A device counts as detached once its `/dev` node has gone, even if
    /// `hdiutil info` still lists it: `hdiutil` lags behind the kernel for a
    /// moment after a detach (and after a drive ejects itself), and then
    /// says "No such file or directory" to a detach of it. That is waited
    /// out, but not counted as a leak.
    ///
    /// `hdiutil info` can lag the other way too, under load: not list yet
    /// an image just attached (task 2.G-b: a forced detach returned at once
    /// with the volume still mounted, and the test read the file it
    /// expected gone). So the image is detached only once neither of its
    /// mount points is one any more ([`mounted`]); until then, it is
    /// detached by mount point.
    fn detach(&self, normally_for: Duration, forced_for: Duration) -> Vec<String> {
        let started = Instant::now();
        let mut backoff = HDIUTIL_FIRST_BACKOFF;
        // What went wrong along the way, reported if anything did.
        let mut trouble = Vec::new();
        let mut showed_holders = false;
        loop {
            let attachments = attachments_of(&self.image, &mut trouble);
            // One device per attachment: detaching it detaches the rest.
            let devices: Vec<String> = attachments
                .iter()
                .filter_map(|attachment| attachment.present.first().cloned())
                .collect();
            let elapsed = started.elapsed();
            let out_of_time = elapsed >= normally_for + forced_for;
            let force = elapsed >= normally_for;
            let still_mounted: Vec<&Path> = [self.mount.as_path(), self.elsewhere.as_path()]
                .into_iter()
                .filter(|point| mounted(point))
                .collect();
            if devices.is_empty() && !still_mounted.is_empty() {
                let points: Vec<String> = still_mounted
                    .iter()
                    .map(|point| point.display().to_string())
                    .collect();
                if out_of_time {
                    eprintln!(
                        "DiskImage: {} still mounted at {} after {elapsed:.1?}, with no \
                         device in hdiutil info{}",
                        self.image.display(),
                        points.join(", "),
                        if trouble.is_empty() {
                            String::new()
                        } else {
                            format!(":\n  {}", trouble.join("\n  "))
                        }
                    );
                    return points;
                }
                trouble.push(format!(
                    "at {elapsed:.1?}, still mounted at {} with no device in hdiutil info; \
                     detaching by mount point",
                    points.join(", ")
                ));
                for point in &still_mounted {
                    let mut command = Command::new("/usr/bin/hdiutil");
                    command.arg("detach").arg(point);
                    if force {
                        command.arg("-force");
                    }
                    match command.output() {
                        Ok(output) if !output.status.success() => trouble
                            .push(format!("{command:?} failed: {}", hdiutil_message(&output))),
                        Ok(_) => {}
                        Err(error) => trouble.push(format!("couldn't run {command:?}: {error}")),
                    }
                }
                if [self.mount.as_path(), self.elsewhere.as_path()]
                    .into_iter()
                    .any(mounted)
                {
                    std::thread::sleep(backoff);
                    backoff = (backoff * 2).min(DETACH_MAX_BACKOFF);
                }
                continue;
            }
            if devices.is_empty() {
                let lagging = attachments.iter().any(|a| !a.listed.is_empty());
                if lagging && !out_of_time {
                    // Gone from /dev, still in `hdiutil info`: let it catch up.
                    std::thread::sleep(backoff);
                    backoff = (backoff * 2).min(DETACH_MAX_BACKOFF);
                    continue;
                }
                if lagging {
                    trouble.push(format!(
                        "after {elapsed:.1?}, hdiutil info still lists {} with no device \
                         left in /dev; counted as detached",
                        self.image.display()
                    ));
                }
                if !trouble.is_empty() {
                    eprintln!(
                        "DiskImage: detached {} in {elapsed:.1?}, after:\n  {}",
                        self.image.display(),
                        trouble.join("\n  ")
                    );
                }
                return devices;
            }
            if out_of_time {
                eprintln!(
                    "DiskImage: couldn't detach {} ({}) in {elapsed:.1?}{}",
                    self.image.display(),
                    devices.join(", "),
                    if trouble.is_empty() {
                        String::new()
                    } else {
                        format!(":\n  {}", trouble.join("\n  "))
                    }
                );
                return devices;
            }
            if force && !showed_holders && !normally_for.is_zero() {
                // An ordinary detach kept failing: say who holds the volume.
                showed_holders = true;
                trouble.push(holders(&attachments));
            }
            for device in &devices {
                let mut command = Command::new("/usr/bin/hdiutil");
                command.arg("detach").arg(device);
                if force {
                    command.arg("-force");
                }
                let at = started.elapsed();
                let output = match command.output() {
                    Ok(output) => output,
                    Err(error) => {
                        trouble.push(format!("couldn't run {command:?}: {error}"));
                        continue;
                    }
                };
                if !output.status.success() {
                    // Not a failure if the device has gone meanwhile: the
                    // next look at `/dev` counts it as detached.
                    let gone = hdiutil_said(&output, DEVICE_GONE) && !in_dev(device);
                    trouble.push(format!(
                        "at {at:.1?}, {command:?} failed after {:.1?}{}: {}",
                        started.elapsed() - at,
                        if gone {
                            " (the device had gone already)"
                        } else {
                            ""
                        },
                        hdiutil_message(&output)
                    ));
                }
            }
            // Checked again straight away: a detach that worked needs no wait.
            if attachments_of(&self.image, &mut trouble)
                .iter()
                .all(|attachment| attachment.present.is_empty())
            {
                continue;
            }
            std::thread::sleep(backoff);
            backoff = (backoff * 2).min(DETACH_MAX_BACKOFF);
        }
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

/// How long [`DiskImage::detach_all`] tries an ordinary detach, and then
/// `-force`, before reporting the image as leaked; and the longest wait
/// between two tries. A volume can be busy for a moment after a file on it
/// is closed (the disk arbitration daemon, or another process, still
/// looking at it); a refused ordinary detach itself takes from a fraction
/// of a second to several.
const DETACH_NORMALLY_FOR: Duration = Duration::from_secs(4);
const DETACH_FORCED_FOR: Duration = Duration::from_secs(6);
const DETACH_MAX_BACKOFF: Duration = Duration::from_secs(1);

/// What `hdiutil detach` says about a device that has already gone.
const DEVICE_GONE: &str = "No such file or directory";

/// One attachment of an image, from `hdiutil info`.
struct Attachment {
    /// The whole-disk devices (`/dev/diskN`, not `/dev/diskNsM`) listed for
    /// it: the disk itself first, then any APFS container on it.
    listed: Vec<String>,
    /// Those of them still in `/dev`. Detaching the first detaches the
    /// rest.
    present: Vec<String>,
    /// Where its volumes are mounted.
    mount_points: Vec<String>,
}

/// The attachments of `image` in `hdiutil info`. Tries `hdiutil info` a
/// few times; if it never works, notes that in `trouble` and returns none.
fn attachments_of(image: &Path, trouble: &mut Vec<String>) -> Vec<Attachment> {
    let mut info = None;
    for _ in 0..3 {
        match Command::new("/usr/bin/hdiutil").arg("info").output() {
            Ok(output) if output.status.success() => {
                info = Some(String::from_utf8_lossy(&output.stdout).into_owned());
                break;
            }
            Ok(output) => trouble.push(format!("hdiutil info failed: {}", describe(&output))),
            Err(error) => trouble.push(format!("couldn't run hdiutil info: {error}")),
        }
        std::thread::sleep(HDIUTIL_FIRST_BACKOFF);
    }
    let Some(info) = info else {
        return Vec::new();
    };
    let image = image.to_string_lossy();
    // One section per attached image, each starting with a line of `=`.
    info.split("\n=")
        .filter(|section| {
            section.lines().any(|line| {
                line.split_once(':')
                    .is_some_and(|(key, value)| key.trim() == "image-path" && value.trim() == image)
            })
        })
        .map(|section| {
            // Device lines: `/dev/disk7s1<TAB>content hint<TAB>mount point`.
            let devices: Vec<Vec<&str>> = section
                .lines()
                .filter(|line| line.starts_with("/dev/disk"))
                .map(|line| line.split('\t').map(str::trim).collect())
                .collect();
            let listed: Vec<String> = devices
                .iter()
                .map(|fields| fields[0])
                .filter(|device| {
                    device["/dev/disk".len()..]
                        .bytes()
                        .all(|b| b.is_ascii_digit())
                })
                .map(str::to_owned)
                .collect();
            let present = listed
                .iter()
                .filter(|device| in_dev(device))
                .cloned()
                .collect();
            let mount_points = devices
                .iter()
                .filter_map(|fields| fields.last().filter(|f| f.starts_with('/')))
                .filter(|point| !point.starts_with("/dev/"))
                .map(|point| (*point).to_owned())
                .collect();
            Attachment {
                listed,
                present,
                mount_points,
            }
        })
        .collect()
}

/// Whether a volume is mounted at `point`: the folder is on another device
/// than its parent. A missing folder isn't (a forced detach can remove the
/// mount point); one that can't be looked at counts as mounted, so a detach
/// keeps trying rather than reporting success it can't see.
fn mounted(point: &Path) -> bool {
    use std::os::unix::fs::MetadataExt as _;
    let Some(parent) = point.parent() else {
        return false;
    };
    match (fs::metadata(point), fs::metadata(parent)) {
        (Err(error), _) if error.kind() == io::ErrorKind::NotFound => false,
        (Ok(here), Ok(above)) => here.dev() != above.dev(),
        _ => true,
    }
}

/// Whether the device node is still in `/dev`. Only "not found" counts as
/// gone: any other error (such as not being allowed to look) doesn't.
fn in_dev(device: &str) -> bool {
    !matches!(fs::metadata(device), Err(error) if error.kind() == io::ErrorKind::NotFound)
}

/// Whether `hdiutil`'s output includes `text`.
fn hdiutil_said(output: &std::process::Output, text: &str) -> bool {
    String::from_utf8_lossy(&output.stdout).contains(text)
        || String::from_utf8_lossy(&output.stderr).contains(text)
}

/// `hdiutil`'s exit status and what it said, without its deprecation
/// warnings, on one line.
fn hdiutil_message(output: &std::process::Output) -> String {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let said: Vec<&str> = stdout
        .lines()
        .chain(stderr.lines())
        .filter(|line| !line.is_empty() && !line.contains("is deprecated"))
        .collect();
    format!("{} ({})", said.join(" / "), output.status)
}

/// The processes with files open on the attachments' volumes (`lsof`),
/// for a report of why an ordinary detach is refused.
fn holders(attachments: &[Attachment]) -> String {
    let points: Vec<&String> = attachments
        .iter()
        .flat_map(|attachment| &attachment.mount_points)
        .collect();
    if points.is_empty() {
        return "an ordinary detach kept failing; no volume is mounted".to_owned();
    }
    let mut command = Command::new("/usr/sbin/lsof");
    command
        .arg("-n")
        .arg("-P")
        .arg("+f")
        .arg("--")
        .args(&points);
    match command.output() {
        Ok(output) => {
            let open = String::from_utf8_lossy(&output.stdout);
            if open.trim().is_empty() {
                format!("an ordinary detach kept failing; lsof finds nothing open on {points:?}")
            } else {
                format!(
                    "an ordinary detach kept failing; open on {points:?}:\n    {}",
                    open.trim_end().replace('\n', "\n    ")
                )
            }
        }
        Err(error) => format!("an ordinary detach kept failing; couldn't run lsof: {error}"),
    }
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

/// The swap (ADR-0012 decision 1): a file that isn't the one checked, put
/// there by another app between the check and the swap, is swapped back and
/// kept; the one checked is replaced, and the folder the new file was made
/// in goes, with the old file in it.
#[test]
fn a_swap_with_a_file_other_than_the_one_checked_is_undone() {
    use std::io::Write as _;
    let dir = TempDir::new("swap-check");
    let temps = dir.temp_folders();
    let dest = dir.file("a.csv", b"old");
    let checked = write::identity_at(&dest).unwrap();
    let other = dir.file("other.csv", b"another app's");
    fs::rename(&other, &dest).unwrap();
    let mut staged = Staged::create(&temps, None, &dest, false).unwrap();
    staged.writer().write_all(b"new").unwrap();
    staged.finish(None).unwrap();
    let _snapshot = staged.snapshot(&temps, None, &|| false).unwrap();
    assert!(matches!(
        staged.swap_into(&dest, Some(&checked)),
        Err(SwapError::Changed)
    ));
    assert_eq!(fs::read(&dest).unwrap(), b"another app's");
    let checked = write::identity_at(&dest).unwrap();
    let put = staged.swap_into(&dest, Some(&checked)).unwrap();
    assert_eq!(put.placed, Placed::Swapped);
    assert_eq!(put.kept, None);
    assert_eq!(fs::read(&dest).unwrap(), b"new");
    drop(staged);
    let mut left: Vec<_> = fs::read_dir(dir.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .filter(|name| name != "scratch" && name != "records")
        .collect();
    left.sort();
    assert_eq!(left, [std::ffi::OsString::from("a.csv")], "{left:?}");
}

/// A swap that took out another app's file and can't swap it back (the
/// volume went, say) has still put the new file in place: the save has
/// succeeded, and the other app's file is kept next to it, under a visible
/// name, never deleted.
#[test]
fn a_file_swapped_out_that_cant_be_swapped_back_is_kept() {
    use std::io::Write as _;
    let dir = TempDir::new("swap-back-fails");
    let temps = dir.temp_folders();
    let dest = dir.file("a.csv", b"old");
    let checked = write::identity_at(&dest).unwrap();
    let other = dir.file("other.csv", b"another app's");
    fs::rename(&other, &dest).unwrap();
    let mut staged = Staged::create(&temps, None, &dest, false).unwrap();
    staged.writer().write_all(b"new").unwrap();
    staged.finish(None).unwrap();
    let _snapshot = staged.snapshot(&temps, None, &|| false).unwrap();
    write::FAIL_SWAP_BACK.with(|fail| fail.set(true));
    let put = staged.swap_into(&dest, Some(&checked));
    write::FAIL_SWAP_BACK.with(|fail| fail.set(false));
    let put = put.unwrap();
    assert_eq!(put.placed, Placed::Swapped);
    let kept = put.kept.unwrap();
    assert_eq!(kept, dir.path().join("a (replaced, kept by Leal).csv"));
    assert_eq!(fs::read(&kept).unwrap(), b"another app's");
    assert_eq!(fs::read(&dest).unwrap(), b"new");
    drop(staged);
    let mut left: Vec<_> = fs::read_dir(dir.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .filter(|name| name != "scratch" && name != "records")
        .collect();
    left.sort();
    assert_eq!(
        left,
        [
            std::ffi::OsString::from("a (replaced, kept by Leal).csv"),
            std::ffi::OsString::from("a.csv"),
        ],
        "{left:?}"
    );
}

/// Something other than a regular file at the destination is never
/// replaced; nor is a regular file swapped out in its place kept there.
#[test]
fn a_swap_never_replaces_a_folder_or_a_pipe() {
    use std::io::Write as _;
    let dir = TempDir::new("swap-not-a-file");
    let temps = dir.temp_folders();
    let folder = dir.path().join("folder.csv");
    fs::create_dir(&folder).unwrap();
    let pipe = dir.path().join("pipe.csv");
    let made = Command::new("/usr/bin/mkfifo").arg(&pipe).status().unwrap();
    assert!(made.success());
    for dest in [&folder, &pipe] {
        let mut staged = Staged::create(&temps, None, dest, false).unwrap();
        staged.writer().write_all(b"new").unwrap();
        staged.finish(None).unwrap();
        let _snapshot = staged.snapshot(&temps, None, &|| false).unwrap();
        assert!(matches!(
            staged.swap_into(dest, None),
            Err(SwapError::NotAFile)
        ));
    }
    assert!(folder.is_dir());
    assert!(std::os::unix::fs::FileTypeExt::is_fifo(
        &fs::symlink_metadata(&pipe).unwrap().file_type()
    ));
}

/// The volume's rename capabilities: the scratch directory's APFS volume
/// swaps and renames exclusively.
#[test]
fn apfs_can_swap_and_rename_exclusively() {
    let dir = TempDir::new("rename-capabilities");
    let capabilities = sys::rename_capabilities(dir.path()).unwrap();
    assert!(capabilities.swap);
    assert!(capabilities.exclusive);
}

/// An attribute the new file can't be given (here an access control list
/// on it denies writing attributes) is skipped and named, never a failed
/// save (ADR-0012 decision 1).
#[test]
fn an_attribute_that_cant_be_set_is_skipped_and_named() {
    let dir = TempDir::new("attribute-skipped");
    let temps = dir.temp_folders();
    let dest = dir.file("a.csv", b"old");
    let tags = b"bplist00\xa1\x01UGreen\n\x08\x0a";
    write_attribute(&dest, c"com.apple.metadata:_kMDItemUserTags", Some(tags));
    let existing = write::look_afresh(&dest).unwrap().unwrap();
    let staged = Staged::create(&temps, None, &dest, false).unwrap();
    let staged_path = staged.path().to_str().unwrap().to_owned();
    let status = Command::new("/bin/chmod")
        .args(["+a", "everyone deny writeextattr", &staged_path])
        .status()
        .unwrap();
    assert!(status.success());
    let skipped = staged.copy_attributes(&existing).unwrap();
    assert_eq!(skipped, ["com.apple.metadata:_kMDItemUserTags"]);
}
