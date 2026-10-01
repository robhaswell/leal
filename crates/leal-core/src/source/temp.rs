//! Leal's temporary folders: where clones and copies go, the record kept of
//! each one, and the cleanup of folders a crash left behind (DESIGN §3.1).
//!
//! # Records
//!
//! Before Leal puts anything in a temporary folder, it writes a *record* in
//! the records folder: a small file named `<id>.record` whose contents are
//! the temporary folder's path. The folder holds one file, `leal-<id>`.
//!
//! The process using the folder keeps its record open with an exclusive
//! `flock(2)` lock. The kernel releases the lock when the process exits,
//! however it exits, so a record that can be locked belongs to a process
//! that has gone. That is how [`TempFolders::remove_leftovers`] tells a
//! crash's leftovers from a folder another running Leal is still using,
//! with no process IDs to go stale.

use std::ffi::OsString;
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// The extension of a record file.
const RECORD_EXTENSION: &str = "record";

/// How old an empty record must be before cleanup deletes it. A record is
/// empty only between being created and being written, a few microseconds,
/// or forever if Leal crashed in between.
const EMPTY_RECORD_GRACE: Duration = Duration::from_secs(60);

/// Where Leal may put temporary files, and where it records them.
///
/// The app passes locations a sandboxed app may write to (DESIGN §4.3):
/// its temporary directory, and a folder in its Application Support
/// directory for the records.
#[derive(Debug, Clone)]
pub struct TempFolders {
    scratch: PathBuf,
    records: PathBuf,
}

impl TempFolders {
    /// Temporary folders go in `scratch` (the app's temporary directory),
    /// unless the app supplies one on the file's own volume, and records go
    /// in `records`. Both are created when first needed.
    pub fn new(scratch: impl Into<PathBuf>, records: impl Into<PathBuf>) -> Self {
        Self {
            scratch: scratch.into(),
            records: records.into(),
        }
    }

    /// The directory clones and copies go in when no folder on the file's
    /// own volume is given.
    #[must_use]
    pub fn scratch(&self) -> &Path {
        &self.scratch
    }

    /// The directory records are kept in.
    #[must_use]
    pub fn records(&self) -> &Path {
        &self.records
    }

    /// Removes the temporary folders that Leal processes which are no longer
    /// running left behind, for example after a crash. The app calls it at
    /// launch. Folders still in use by a running Leal are left alone.
    ///
    /// It checks every recorded folder, on any volume. A folder on a volume
    /// that isn't mounted now keeps its record, so a later launch can remove
    /// it. Only Leal's own file in each folder is deleted, then the folder
    /// if that leaves it empty: nothing else is ever removed.
    ///
    /// Returns how many folders it removed.
    ///
    /// # Errors
    ///
    /// Returns the error if the records directory exists but can't be
    /// listed. Problems with single records are skipped, to be tried again
    /// at the next launch.
    pub fn remove_leftovers(&self) -> io::Result<usize> {
        let entries = match fs::read_dir(&self.records) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(0),
            Err(error) => return Err(error),
        };
        let mut removed = 0;
        for entry in entries {
            let Ok(entry) = entry else { continue };
            let path = entry.path();
            if path.extension().is_some_and(|ext| ext == RECORD_EXTENSION)
                && remove_if_abandoned(&path)
            {
                removed += 1;
            }
        }
        Ok(removed)
    }

    /// Records and takes over `folder`, an existing empty folder that the
    /// app made for this file (on the file's own volume). Dropping the
    /// result deletes it.
    pub(super) fn adopt(&self, folder: PathBuf) -> io::Result<TempFolder> {
        let id = RecordId::new();
        let (record, record_path) = self.write_record(&id, &folder)?;
        Ok(TempFolder::new(folder, &id, record, record_path))
    }

    /// Records and creates a new folder in the scratch directory. Dropping
    /// the result deletes it.
    pub(super) fn create(&self) -> io::Result<TempFolder> {
        fs::create_dir_all(&self.scratch)?;
        // The record is written before the folder is made. A crash in
        // between leaves a record of a folder that doesn't exist, which
        // cleanup simply deletes.
        let id = RecordId::new();
        let folder = self.scratch.join(format!("leal-{id}"));
        let (record, record_path) = self.write_record(&id, &folder)?;
        let temp = TempFolder::new(folder, &id, record, record_path);
        DirBuilder::new().mode(0o700).create(&temp.folder)?;
        Ok(temp)
    }

    /// Creates the record `<id>.record`, locks it and writes `folder` in it.
    /// Returns the open record and its path.
    fn write_record(&self, id: &RecordId, folder: &Path) -> io::Result<(File, PathBuf)> {
        fs::create_dir_all(&self.records)?;
        let path = self.records.join(format!("{id}.{RECORD_EXTENSION}"));
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)?;
        // Another process's cleanup may open the new, still empty record and
        // lock it for a moment; it leaves empty records alone (see
        // `remove_if_abandoned`), so this only waits for it to let go.
        let written = file
            .lock()
            .and_then(|()| file.write_all(folder.as_os_str().as_bytes()));
        if let Err(error) = written {
            let _ = fs::remove_file(&path);
            return Err(error);
        }
        Ok((file, path))
    }
}

/// A unique ID for a record and its folder: the process ID, the time in
/// nanoseconds and a per-process counter, so two processes (or two opens in
/// one) never pick the same one.
struct RecordId(String);

impl RecordId {
    fn new() -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let count = COUNTER.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |time| time.as_nanos());
        Self(format!("{}-{nanos}-{count}", std::process::id()))
    }
}

impl std::fmt::Display for RecordId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A recorded temporary folder, owned by one [`Source`](super::Source).
///
/// It holds its record open and locked. Dropping it deletes the file in the
/// folder, the folder, and then the record; that is the "clones are deleted
/// when the document closes" of DESIGN §3.1.
#[derive(Debug)]
pub(super) struct TempFolder {
    folder: PathBuf,
    /// The name of the one file Leal puts in the folder: `leal-<id>`.
    file_name: OsString,
    record_path: PathBuf,
    /// Kept open so the lock lasts as long as the folder is in use.
    record: Option<File>,
}

impl TempFolder {
    fn new(folder: PathBuf, id: &RecordId, record: File, record_path: PathBuf) -> Self {
        Self {
            folder,
            file_name: OsString::from(format!("leal-{id}")),
            record_path,
            record: Some(record),
        }
    }

    /// The path of the one file Leal puts in this folder (the clone or the
    /// copy). It doesn't exist until something creates it.
    pub(super) fn file_path(&self) -> PathBuf {
        self.folder.join(&self.file_name)
    }

    /// The folder itself.
    #[cfg(test)]
    pub(super) fn folder(&self) -> &Path {
        &self.folder
    }

    /// The record's path.
    #[cfg(test)]
    pub(super) fn record_path(&self) -> &Path {
        &self.record_path
    }

    /// Lets go of the folder as a crash would: the record's lock is released
    /// and nothing is deleted.
    #[cfg(test)]
    pub(super) fn abandon(mut self) {
        drop(self.record.take());
        std::mem::forget(self);
    }
}

impl Drop for TempFolder {
    fn drop(&mut self) {
        // Drop can't return an error. If the folder can't be removed now,
        // its record stays, and cleanup at the next launch tries again.
        if remove_folder(&self.folder, &self.file_name) {
            let _ = fs::remove_file(&self.record_path);
        }
        // Closing the record releases its lock.
        drop(self.record.take());
    }
}

/// Deletes Leal's file `file_name` in `folder`, then `folder` itself if
/// that leaves it empty. Returns `true` if the folder is gone (or was never
/// made), `false` if it is still there or might be (its volume isn't
/// mounted).
fn remove_folder(folder: &Path, file_name: &OsString) -> bool {
    match fs::remove_file(folder.join(file_name)) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(_) => return false,
    }
    match fs::remove_dir(folder) {
        Ok(()) => true,
        // Gone already, unless its whole volume is missing: then it may
        // still exist on a volume that isn't mounted now.
        Err(error) if error.kind() == io::ErrorKind::NotFound => !on_missing_volume(folder),
        // Not empty, or not removable: leave it.
        Err(_) => false,
    }
}

/// Whether `path` is on an external volume that isn't mounted now: macOS
/// mounts volumes at `/Volumes/<name>`, and that folder is missing.
fn on_missing_volume(path: &Path) -> bool {
    let volumes = Path::new("/Volumes");
    let Ok(rest) = path.strip_prefix(volumes) else {
        return false;
    };
    rest.components()
        .next()
        .is_some_and(|name| !volumes.join(name).exists())
}

/// Removes the folder recorded at `record_path`, and the record, if the
/// process that made it has gone. Returns `true` if a folder was removed.
fn remove_if_abandoned(record_path: &Path) -> bool {
    let Some(id) = record_path.file_stem().map(OsString::from) else {
        return false;
    };
    let Ok(mut record) = OpenOptions::new().read(true).write(true).open(record_path) else {
        return false;
    };
    // A running Leal holds the lock on each record it uses.
    if record.try_lock().is_err() {
        return false;
    }
    let mut contents = Vec::new();
    if record.read_to_end(&mut contents).is_err() {
        return false;
    }
    if contents.is_empty() {
        // Being written right now, or abandoned before it was written: in
        // either case there is no folder to remove.
        let old = record
            .metadata()
            .and_then(|metadata| metadata.modified())
            .ok()
            .and_then(|modified| modified.elapsed().ok())
            .is_some_and(|age| age > EMPTY_RECORD_GRACE);
        if old {
            let _ = fs::remove_file(record_path);
        }
        return false;
    }
    let folder = PathBuf::from(OsString::from_vec(contents));
    let mut file_name = OsString::from("leal-");
    file_name.push(&id);
    let existed = folder.exists();
    if !remove_folder(&folder, &file_name) {
        return false;
    }
    // Delete the record while still holding its lock, so no other cleanup
    // can act on it in between.
    let _ = fs::remove_file(record_path);
    existed
}
