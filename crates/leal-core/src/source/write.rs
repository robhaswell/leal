//! Writing the file a save makes (task 2.2, DESIGN §3.7, ADR-0012
//! decision 1): a new file next to the destination, which replaces it only
//! once it is complete.
//!
//! - [`Staged::create`] makes the new file (mode `0600` until it is
//!   finished) in a recorded temporary folder on the destination's volume:
//!   the one the app made there (its item-replacement folder), or, if it
//!   gave none or one on another volume, a hidden `.leal-save-<id>` folder
//!   in the destination's own folder. On a volume that can vanish (a
//!   removable drive, a share) it also tees every byte into a copy on the
//!   internal disk, the document's next snapshot, so the file is never read
//!   back from that volume.
//! - The save writes the bytes ([`Staged::writer`]), then gives the new
//!   file what the old one had, best-effort, by an explicit policy
//!   (ADR-0012 decision 1):
//!   1. each extended attribute on its own ([`Staged::copy_attributes`]):
//!      those the system keeps for a safe save
//!      (`XATTR_OPERATION_INTENT_SAVE`, which drops quarantine and
//!      attributes tied to the contents), plus Finder's info and the
//!      resource fork, which it leaves to copy engines; never Leal's own
//!      two, the compression header, or the attributes the system protects
//!      or derives (`macl`, `provenance`, `rootless`, `kMDLabel_*`). One
//!      that can't be set (`EPERM`, `EACCES`, `ENOTSUP`, `E2BIG`, `EINVAL`)
//!      is skipped and named;
//!   2. Leal's own two attributes ([`Staged::set_attribute`]);
//!   3. ([`Staged::finish`]) the owner and group, the creation date, the
//!      mode, the user-settable flags (`UF_SETTABLE`, never
//!      `UF_COMPRESSED`, `UF_DATAVAULT`, the locking flags or any `SF_*`),
//!      and last the access control list, so an entry denying attribute or
//!      permission writes can't stop anything before it; then the bytes are
//!      ordered onto the disk (`F_BARRIERFSYNC`).
//! - [`Staged::snapshot`] makes the document's next snapshot of the new
//!   file (the tee copy, or a clone of it), and closes it, before it goes
//!   into place.
//! - [`Staged::swap_into`] puts it in place, the way the destination's
//!   volume allows (`getattrlist(ATTR_VOL_CAPABILITIES)`): where it can swap
//!   (`VOL_CAP_INT_RENAME_SWAP`: APFS), `renamex_np(RENAME_SWAP)`, then the
//!   file swapped out is compared with the one checked, and swapped back if
//!   another app changed it in between. Elsewhere (HFS+, exFAT, FAT), a
//!   plain `rename(2)` over it, with no check after (the check just before,
//!   under the watcher's lock, still holds). A new place gets
//!   `RENAME_EXCL` where supported, and otherwise a look that nothing is
//!   there first.
//!
//! Until the new file is in place, dropping the [`Staged`] (a cancelled or
//! failed save) deletes it and its folder, and the destination is
//! untouched. Once it is in place the save has succeeded, whatever else
//! goes wrong. The old file a swap leaves in the folder is then deleted,
//! unless it couldn't be checked or wasn't the one checked and couldn't be
//! swapped back: it may be another app's version, so it is kept next to the
//! file under a visible name ([`Put::kept`]). A crash leaves the folder's
//! record, so the next launch's cleanup removes what's left; a crash between
//! a swap and its check loses an old version the check would have kept.
//!
//! [`look_afresh`] is the check before writing (ADR-0008 decision 9): the
//! file at the destination, opened afresh and looked at with `fstat`, which
//! makes a network file system ask its server.

use std::ffi::CStr;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use super::temp::{LOCKING_FLAGS, TempFolder};
use super::{
    FileIdentity, GivenFolder, OpenErrorKind, Storage, TempFolders, clone, open_regular, sys,
};

/// Attributes kept on save although the system leaves them to copy
/// engines: Finder's info (type, creator, label, the custom-icon bit) and
/// the resource fork (an old custom icon).
const KEEP: [&CStr; 2] = [c"com.apple.FinderInfo", c"com.apple.ResourceFork"];

/// Attributes never copied: Leal's own two, which the save writes itself;
/// the compression header, which describes the old file's blocks; and the
/// ones the system sets or derives itself.
const NEVER: [&CStr; 6] = [
    super::TEXT_ENCODING_ATTRIBUTE_C,
    super::INTERPRETATION_ATTRIBUTE_C,
    c"com.apple.decmpfs",
    c"com.apple.macl",
    c"com.apple.provenance",
    c"com.apple.rootless",
];

/// Spotlight's own labels, which it derives: never copied.
const NEVER_PREFIX: &[u8] = b"com.apple.metadata:kMDLabel_";

/// `UF_DATAVAULT`: the system's own flag for protected data; never copied.
const UF_DATAVAULT: u32 = 0x0000_0080;

/// The errors that make a piece of metadata skipped rather than the save
/// failed: not allowed, not supported here, or too big for this volume.
fn skippable(error: &io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(libc::EPERM | libc::EACCES | libc::ENOTSUP | libc::E2BIG | libc::EINVAL)
    )
}

/// The user's file, opened afresh: what a save compares with what Leal
/// opened, and copies the metadata of.
#[derive(Debug)]
pub struct Existing {
    file: File,
    metadata: fs::Metadata,
    identity: FileIdentity,
}

impl Existing {
    /// Which file it is, and its size and modification time now.
    #[must_use]
    pub fn identity(&self) -> &FileIdentity {
        &self.identity
    }

    /// Whether it is locked: immutable (Finder's Locked, `uchg`, or
    /// `schg`) or append-only. A save can't replace it.
    #[must_use]
    pub fn is_locked(&self) -> bool {
        std::os::macos::fs::MetadataExt::st_flags(&self.metadata) & LOCKING_FLAGS != 0
    }

    /// When its metadata (permissions, attributes, flags) last changed:
    /// its status change time.
    #[must_use]
    pub fn changed(&self) -> (i64, i64) {
        (self.metadata.ctime(), self.metadata.ctime_nsec())
    }

    /// Sets its extended attribute `name` to `value` (`fsetxattr`): on the
    /// file opened, whatever is at its path by now.
    ///
    /// # Errors
    ///
    /// If the system refuses: no permission, or not supported there.
    pub fn set_attribute(&self, name: &CStr, value: &[u8]) -> io::Result<()> {
        sys::set_xattr(&self.file, name, value)
    }
}

/// The file at `path`, opened afresh and looked at with `fstat`
/// (ADR-0008 decision 9): `None` if nothing is there.
///
/// # Errors
///
/// If something is there but can't be opened, or isn't a regular file.
pub fn look_afresh(path: &Path) -> io::Result<Option<Existing>> {
    match open_regular(path) {
        Ok((file, identity)) => {
            let metadata = file.metadata()?;
            Ok(Some(Existing {
                file,
                metadata,
                identity,
            }))
        }
        Err(error) if error.kind() == OpenErrorKind::NotFound => Ok(None),
        // The errno kept, so a caller can tell a permission error from the
        // rest (phase 2 gate); the message only where there is none.
        Err(error) => Err(match error.raw_os_error() {
            Some(code) => io::Error::from_raw_os_error(code),
            None => io::Error::new(error.io_error().kind(), error.to_string()),
        }),
    }
}

/// Whether this process may write the file at `path` (`faccessat` with
/// `AT_EACCESS`): a rename needs only the folder to be writable, so a save
/// that respects the file's own permissions asks first.
///
/// # Errors
///
/// If the check itself fails, other than by saying no.
pub fn can_write(path: &Path) -> io::Result<bool> {
    sys::can_write(path)
}

/// Whether `now` is the file `then` describes, unchanged: the same inode,
/// size and modification time. Not the device, which changes each time a
/// removable volume is mounted.
#[must_use]
pub fn same_file(then: &FileIdentity, now: &FileIdentity) -> bool {
    then.inode == now.inode && then.len == now.len && then.modified == now.modified
}

fn identity_of(metadata: &fs::Metadata) -> FileIdentity {
    FileIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
        len: metadata.len(),
        modified: metadata.modified().ok(),
    }
}

/// The identity of the file at `path` now: opened afresh and `fstat`ed, so
/// a network file system revalidates it, or, if it can't be opened (a
/// write-only file), `stat`ed.
///
/// # Errors
///
/// If nothing can be learned about it.
pub fn identity_at(path: &Path) -> io::Result<FileIdentity> {
    let metadata = match File::open(path) {
        Ok(file) => file.metadata()?,
        Err(_) => fs::metadata(path)?,
    };
    Ok(identity_of(&metadata))
}

/// `path` with its last part spelled as the volume has it: after a save on
/// a volume that ignores case, the name the file already had, not the one
/// asked for (`A.CSV` for `a.csv`). `path` itself if that can't be learned.
///
/// It lists the folder (`F_GETPATH` gives back the name asked for): an
/// entry of exactly that name, or else the one that is the same file.
#[must_use]
pub fn as_on_disk(path: &Path) -> PathBuf {
    use std::os::unix::fs::DirEntryExt;
    let (Some(name), Ok(metadata)) = (path.file_name(), fs::metadata(path)) else {
        return path.to_owned();
    };
    let Ok(entries) = fs::read_dir(parent_of(path)) else {
        return path.to_owned();
    };
    let mut same_file = None;
    for entry in entries.flatten() {
        let entry_name = entry.file_name();
        if entry_name == name {
            return path.to_owned();
        }
        if same_file.is_none() && entry.ino() == metadata.ino() {
            same_file = Some(entry_name);
        }
    }
    same_file.map_or_else(|| path.to_owned(), |name| path.with_file_name(name))
}

/// What is at `path` itself, a symbolic link not followed: its identity,
/// and whether it is a regular file. Opened afresh where it can be
/// (`O_NOFOLLOW`), otherwise `lstat`ed.
fn entry_at(path: &Path) -> io::Result<(FileIdentity, bool)> {
    let metadata = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file.metadata()?,
        Err(_) => fs::symlink_metadata(path)?,
    };
    Ok((identity_of(&metadata), metadata.is_file()))
}

/// Why [`Staged::swap_into`] didn't put the new file in place. The
/// destination is then as it was.
#[derive(Debug)]
pub enum SwapError {
    /// The file there wasn't the one checked: it changed in between. A swap
    /// has been swapped back.
    Changed,
    /// Nothing was at the destination, for Save.
    Missing,
    /// Something other than a regular file is at the destination.
    NotAFile,
    /// The destination is locked (`EPERM`).
    Locked,
    /// Leal may not replace it (`EACCES`).
    NotWritable,
    /// The swap or rename failed otherwise.
    Io(io::Error),
}

/// How the new file went into place (ADR-0012 decision 1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placed {
    /// Swapped with the old file, which was checked after the swap.
    Swapped,
    /// Renamed over it (or to a new place), because the volume can't swap.
    Renamed,
}

/// The new file, in place: how, its identity, and any old file kept.
#[derive(Debug)]
pub struct Put {
    /// How it went into place.
    pub placed: Placed,
    /// Its identity now (a fresh look after it is in place, or, if that
    /// fails, the one taken before it was closed).
    pub identity: FileIdentity,
    /// The old file a swap took out, when it couldn't be checked, or wasn't
    /// the one checked and couldn't be swapped back: another app's version,
    /// maybe, kept here rather than deleted.
    pub kept: Option<PathBuf>,
}

/// The document's next snapshot, made by [`Staged::snapshot`]: a file in
/// a temporary folder, read-only, ready to map.
#[derive(Debug)]
pub struct Snapshot {
    pub(super) folder: TempFolder,
    pub(super) storage: Storage,
}

/// The new file and its tee copy, as one writer.
#[derive(Debug)]
pub struct Tee<'a> {
    file: &'a File,
    copy: Option<&'a File>,
}

impl Write for Tee<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = (&mut &*self.file).write(buf)?;
        if let Some(copy) = self.copy {
            (&mut &*copy).write_all(&buf[..n])?;
        }
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// A new file being written for a save: see the module docs.
#[derive(Debug)]
pub struct Staged {
    /// Open until [`snapshot`](Self::snapshot) closes it.
    file: Option<File>,
    path: PathBuf,
    /// The copy on the internal disk, for a destination that can vanish.
    tee: Option<(File, TempFolder)>,
    /// The mode a new file gets here: `0666` less the umask.
    new_mode: u32,
    /// Its identity as it was closed.
    closed: Option<FileIdentity>,
    /// Deleted (with whatever file is in it) when dropped, unless kept.
    /// Last, so the file is closed first.
    folder: Option<TempFolder>,
}

impl Staged {
    /// Makes an empty new file (mode `0600`) for a save to `destination`:
    /// in `folder`, an empty folder on the destination's volume, which this
    /// takes over and deletes afterwards; or, with none, or one on another
    /// volume (removed then), in a hidden folder made in the destination's
    /// own folder. With `tee`, also a copy in the scratch directory that
    /// every byte written goes to as well.
    ///
    /// # Errors
    ///
    /// If the folder can't be recorded or made, or a file created.
    pub fn create(
        temps: &TempFolders,
        folder: Option<PathBuf>,
        destination: &Path,
        tee: bool,
    ) -> io::Result<Staged> {
        let parent = parent_of(destination);
        let device = fs::metadata(parent)?.dev();
        let given = folder.map(GivenFolder);
        let folder = match given {
            Some(given) if fs::metadata(&given.0).is_ok_and(|m| m.dev() == device) => {
                let folder = temps.adopt(given.0.clone())?;
                given.release();
                folder
            }
            // On another volume, a rename from it would fail: it is removed
            // (if empty) as `given` drops.
            _ => temps.create_in(parent)?,
        };
        let path = folder.file_path();
        // Made as any new file is (`0666` less the umask), which says what
        // mode a new file gets; then `0600` until it is finished.
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o666)
            .open(&path)?;
        let new_mode = file.metadata()?.mode() & 0o777;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
        let tee = if tee {
            let copy = temps.create()?;
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(copy.file_path())?;
            Some((file, copy))
        } else {
            None
        };
        Ok(Staged {
            file: Some(file),
            path,
            tee,
            new_mode,
            closed: None,
            folder: Some(folder),
        })
    }

    /// The new file (and its tee copy), to write the bytes to. Wrap it in a
    /// `BufWriter`.
    pub fn writer(&self) -> Tee<'_> {
        Tee {
            file: self.open_file(),
            copy: self.tee.as_ref().map(|(file, _)| file),
        }
    }

    /// The new file's path, for tests.
    #[cfg(test)]
    pub(super) fn path(&self) -> &Path {
        &self.path
    }

    fn open_file(&self) -> &File {
        // `file` is `None` only after `snapshot`, which takes `self` by
        // `&mut` and ends the writing; nothing calls this after it.
        self.file
            .as_ref()
            .unwrap_or_else(|| unreachable!("the new file is closed"))
    }

    /// Copies `like`'s extended attributes, one by one, by the policy in the
    /// module docs. Returns the names of those skipped.
    ///
    /// # Errors
    ///
    /// If one can't be read or set for a reason other than the skippable
    /// ones.
    pub fn copy_attributes(&self, like: &Existing) -> io::Result<Vec<String>> {
        copy_attributes(&like.file, self.open_file())
    }

    /// Sets the extended attribute `name` to `value`, or removes it
    /// (`None`).
    ///
    /// # Errors
    ///
    /// If the volume doesn't support extended attributes, for example.
    pub fn set_attribute(&self, name: &CStr, value: Option<&[u8]>) -> io::Result<()> {
        match value {
            Some(value) => sys::set_xattr(self.open_file(), name, value),
            None => sys::remove_xattr(self.open_file(), name),
        }
    }

    /// Gives the new file `like`'s owner and group (as far as Leal may),
    /// creation date, mode, user-settable flags and, last, access control
    /// list; or, for a new file (`None`), the mode a new file gets. Then
    /// orders its bytes onto the disk (`F_BARRIERFSYNC`). Returns what was
    /// skipped.
    ///
    /// # Errors
    ///
    /// If something can't be set for a reason other than the skippable ones,
    /// or the bytes can't be flushed.
    pub fn finish(&self, like: Option<&Existing>) -> io::Result<Vec<String>> {
        let file = self.open_file();
        let skipped = apply_metadata(file, like, self.new_mode)?;
        sys::barrier_sync(file)?;
        Ok(skipped)
    }

    /// The old file's metadata changed while the save ran: gives the new
    /// one (now closed) its metadata again, from `like`, the file as it is
    /// now, with Leal's own `attributes` kept. Returns what was skipped.
    ///
    /// # Errors
    ///
    /// As for [`copy_attributes`](Self::copy_attributes) and
    /// [`finish`](Self::finish).
    pub fn copy_metadata_again(
        &mut self,
        like: &Existing,
        attributes: &[(&CStr, Option<&[u8]>)],
    ) -> io::Result<Vec<String>> {
        // Its owner may change its mode by path whatever the mode is, but
        // may not open it for reading under a mode without read (`0o200`,
        // copied from the old file): so the mode first, then the open.
        fs::set_permissions(&self.path, fs::Permissions::from_mode(0o600))?;
        let file = File::open(&self.path)?;
        // Copied again from `like`, last, by `apply_metadata`; where the
        // volume has none, nothing to clear.
        let _ = sys::clear_acl(&file);
        let mut skipped = Vec::new();
        for name in sys::list_xattrs(&file)? {
            let name: &CStr = &name;
            // Leal's own two are set below; the system's own stay.
            let system = name.to_bytes().starts_with(NEVER_PREFIX) || NEVER.contains(&name);
            if system || attributes.iter().any(|(ours, _)| *ours == name) {
                continue;
            }
            match sys::remove_xattr(&file, name) {
                Ok(()) => {}
                Err(error) if skippable(&error) => {
                    skipped.push(name.to_string_lossy().into_owned());
                }
                Err(error) => return Err(error),
            }
        }
        skipped.extend(copy_attributes(&like.file, &file)?);
        for &(name, value) in attributes {
            let set = match value {
                Some(value) => sys::set_xattr(&file, name, value),
                None => sys::remove_xattr(&file, name),
            };
            if set.is_err() {
                skipped.push(name.to_string_lossy().into_owned());
            }
        }
        skipped.extend(apply_metadata(&file, Some(like), self.new_mode)?);
        sys::barrier_sync(&file)?;
        self.closed = Some(identity_of(&file.metadata()?));
        Ok(skipped)
    }

    /// Makes the document's next snapshot of the new file, and closes the
    /// new file: the tee copy, if there is one; otherwise a clone (in
    /// `folder`, an empty folder on the destination's volume the app made,
    /// or the scratch directory); otherwise a copy read back from it, a
    /// chunk at a time, stopping with `Interrupted` once `cancelled` says
    /// so.
    ///
    /// # Errors
    ///
    /// If none of these can be made.
    pub fn snapshot(
        &mut self,
        temps: &TempFolders,
        folder: Option<PathBuf>,
        cancelled: &dyn Fn() -> bool,
    ) -> io::Result<Snapshot> {
        let snapshot = if let Some((copy, tee)) = self.tee.take() {
            // The tee copy is Leal's own scratch file: no flush needed.
            drop(copy);
            fs::set_permissions(tee.file_path(), fs::Permissions::from_mode(0o400))?;
            Snapshot {
                folder: tee,
                storage: Storage::Copy,
            }
        } else {
            let file = File::open(&self.path)?;
            let as_io = |error: super::OpenError| io::Error::other(error);
            match clone(&file, &self.path, temps, folder.map(GivenFolder)).map_err(as_io)? {
                Some(folder) => Snapshot {
                    folder,
                    storage: Storage::Clone,
                },
                None => Snapshot {
                    folder: copy_to_scratch(&file, temps, cancelled)?,
                    storage: Storage::Copy,
                },
            }
        };
        // The new file is complete: close it before it goes into place, so
        // a file system that updates a file's details when it is closed
        // (SMB) has done so before its identity is taken.
        if let Some(file) = self.file.take() {
            self.closed = Some(identity_of(&file.metadata()?));
        }
        Ok(snapshot)
    }

    /// Puts the new file at `destination` (see the module docs). With
    /// `checked`, a file must be there, and, where the volume can swap, it
    /// is swapped and the file swapped out compared with `checked`, and
    /// swapped back if it differs. Without, the new file replaces whatever
    /// regular file is there, or goes to a new place.
    ///
    /// Once the new file is in place this succeeds, whatever else fails.
    ///
    /// # Errors
    ///
    /// A [`SwapError`]: then the destination is as it was.
    pub fn swap_into(
        &mut self,
        destination: &Path,
        checked: Option<&FileIdentity>,
    ) -> Result<Put, SwapError> {
        let capabilities =
            sys::rename_capabilities(parent_of(destination)).unwrap_or(sys::RenameCapabilities {
                swap: false,
                exclusive: false,
            });
        let failed = |error: io::Error| match error.raw_os_error() {
            Some(libc::EPERM) => SwapError::Locked,
            Some(libc::EACCES) => SwapError::NotWritable,
            _ => SwapError::Io(error),
        };
        let (placed, kept) = match entry_at(destination) {
            Ok((_, false)) => return Err(SwapError::NotAFile),
            Ok(_) if capabilities.swap => {
                sys::swap(&self.path, destination).map_err(failed)?;
                // In place: from here the save has succeeded.
                let out = entry_at(&self.path);
                let fits = match &out {
                    Ok((identity, regular)) => {
                        *regular && checked.is_none_or(|checked| same_file(checked, identity))
                    }
                    Err(_) => false,
                };
                if !fits && out.is_ok() && swap_back(&self.path, destination).is_ok() {
                    return Err(match out {
                        Ok((_, false)) => SwapError::NotAFile,
                        _ => SwapError::Changed,
                    });
                }
                let kept = (!fits).then(|| self.keep_old(destination, capabilities.exclusive));
                (Placed::Swapped, kept)
            }
            Ok(_) => {
                fs::rename(&self.path, destination).map_err(failed)?;
                (Placed::Renamed, None)
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                if checked.is_some() {
                    return Err(SwapError::Missing);
                }
                if capabilities.exclusive {
                    match sys::rename_new(&self.path, destination) {
                        Ok(()) => {}
                        // Something appeared there meanwhile: Save As
                        // replaces whatever regular file is at the place the
                        // user chose.
                        Err(error) if error.raw_os_error() == Some(libc::EEXIST) => {
                            return self.swap_into(destination, None);
                        }
                        Err(error) => return Err(failed(error)),
                    }
                } else {
                    // Looked at just above: nothing there.
                    fs::rename(&self.path, destination).map_err(failed)?;
                }
                (Placed::Renamed, None)
            }
            Err(error) => return Err(SwapError::Io(error)),
        };
        let identity = identity_at(destination)
            .ok()
            .or(self.closed)
            .unwrap_or(FileIdentity {
                device: 0,
                inode: 0,
                len: 0,
                modified: None,
            });
        Ok(Put {
            placed,
            identity,
            kept: kept.flatten(),
        })
    }

    /// Keeps the old file a swap left in the new file's folder, next to
    /// `destination` under a visible name, or, if it can't be moved there,
    /// in that folder (which is then never cleaned up) under
    /// `destination`'s own name. Returns where it is.
    fn keep_old(&mut self, destination: &Path, exclusive: bool) -> Option<PathBuf> {
        let parent = parent_of(destination);
        for n in 1..100 {
            let label = if n == 1 {
                " (replaced, kept by Leal)".to_owned()
            } else {
                format!(" (replaced, kept by Leal {n})")
            };
            let candidate = parent.join(kept_name(destination, &label));
            let moved = if exclusive {
                sys::rename_new(&self.path, &candidate)
            } else if fs::symlink_metadata(&candidate).is_ok() {
                Err(io::Error::from_raw_os_error(libc::EEXIST))
            } else {
                fs::rename(&self.path, &candidate)
            };
            match moved {
                Ok(()) => return Some(candidate),
                Err(error) if error.raw_os_error() == Some(libc::EEXIST) => {}
                Err(_) => break,
            }
        }
        // The staged file keeps its recorded name (`leal-<id>`) until here:
        // the next launch's cleanup deletes a crashed save's file by it.
        let kept = self.folder.take()?.keep();
        let named = kept.with_file_name(kept_name(destination, ""));
        Some(if fs::rename(&kept, &named).is_ok() {
            named
        } else {
            kept
        })
    }
}

/// The longest file name a volume takes, in bytes (`NAME_MAX`).
const NAME_MAX: usize = 255;

/// `destination`'s file name with `suffix` before its extension, its stem
/// shortened (at a character boundary) so the whole fits in [`NAME_MAX`]
/// bytes; without the extension if the suffix and it alone wouldn't fit.
fn kept_name(destination: &Path, suffix: &str) -> String {
    let lossy = |name: &std::ffi::OsStr| name.to_string_lossy().into_owned();
    let stem = destination
        .file_stem()
        .map_or_else(|| "file".to_owned(), lossy);
    let mut extension = destination
        .extension()
        .map_or_else(String::new, |extension| format!(".{}", lossy(extension)));
    if suffix.len() + extension.len() >= NAME_MAX {
        extension.clear();
    }
    let room = NAME_MAX.saturating_sub(suffix.len() + extension.len());
    let mut end = stem.len().min(room);
    while !stem.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{suffix}{extension}", &stem[..end])
}

#[cfg(test)]
thread_local! {
    /// TEST HOOK: a swap back on this thread fails, as if the volume had
    /// gone.
    pub(super) static FAIL_SWAP_BACK: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Swaps the file swapped out back into place.
fn swap_back(path: &Path, destination: &Path) -> io::Result<()> {
    #[cfg(test)]
    if FAIL_SWAP_BACK.with(std::cell::Cell::get) {
        return Err(io::Error::from_raw_os_error(libc::EIO));
    }
    sys::swap(path, destination)
}

/// The folder `path` is in.
fn parent_of(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
}

/// Copies `from`'s extended attributes to `to` by the policy (module docs).
fn copy_attributes(from: &File, to: &File) -> io::Result<Vec<String>> {
    let mut skipped = Vec::new();
    for name in sys::list_xattrs(from)? {
        let name: &CStr = &name;
        let derived = name.to_bytes().starts_with(NEVER_PREFIX);
        if derived || NEVER.contains(&name) || !(KEEP.contains(&name) || sys::keep_on_save(name)) {
            continue;
        }
        let copied = sys::read_whole_xattr(from, name).and_then(|value| match value {
            Some(value) => sys::set_xattr(to, name, &value),
            None => Ok(()),
        });
        match copied {
            Ok(()) => {}
            Err(error) if skippable(&error) => skipped.push(name.to_string_lossy().into_owned()),
            Err(error) => return Err(error),
        }
    }
    Ok(skipped)
}

/// Gives `file` `like`'s owner and group, creation date, mode, flags and
/// access control list, in that order, or `new_mode` for a new file.
fn apply_metadata(file: &File, like: Option<&Existing>, new_mode: u32) -> io::Result<Vec<String>> {
    let mut skipped = Vec::new();
    let mut note = |what: &str, result: io::Result<()>| match result {
        Ok(()) => Ok(()),
        Err(error) if skippable(&error) => {
            skipped.push(what.to_owned());
            Ok(())
        }
        Err(error) => Err(error),
    };
    let Some(like) = like else {
        note(
            "permissions",
            file.set_permissions(fs::Permissions::from_mode(new_mode)),
        )?;
        return Ok(skipped);
    };
    let (uid, gid) = (like.metadata.uid(), like.metadata.gid());
    let mine = file.metadata()?;
    if (mine.uid(), mine.gid()) != (uid, gid) {
        // Only the owner (or root) can give a file away; another group
        // needs membership. Named if it can't be done.
        note(
            "owner",
            std::os::unix::fs::fchown(file, Some(uid), Some(gid)),
        )?;
    }
    if let Some(created) = like
        .metadata
        .created()
        .ok()
        .filter(|&c| has_creation_date(c))
    {
        note("creation date", sys::set_creation_time(file, created))?;
    }
    note(
        "permissions",
        file.set_permissions(fs::Permissions::from_mode(like.metadata.mode() & 0o7777)),
    )?;
    let flags = std::os::macos::fs::MetadataExt::st_flags(&like.metadata)
        & libc::UF_SETTABLE
        & !(libc::UF_COMPRESSED | UF_DATAVAULT | LOCKING_FLAGS);
    if flags != 0 {
        note("flags", sys::set_flags_of(file, flags))?;
    }
    note("access control list", sys::copy_acl(&like.file, file))?;
    Ok(skipped)
}

/// Whether `created` is a real creation date. A volume without creation
/// dates reports a birthtime of exactly -1 second (1969-12-31 23:59:59),
/// which isn't copied onto a new file: that would show as a date a second
/// before 1970. (0 is no sentinel: a file made then has that date, and the
/// code before the pre-1970 fix never treated it as one.)
pub(crate) fn has_creation_date(created: std::time::SystemTime) -> bool {
    created != std::time::UNIX_EPOCH - std::time::Duration::from_secs(1)
}

/// A copy of `file` in a new folder in the scratch directory, a chunk at a
/// time, stopping (`Interrupted`) once `cancelled` says so.
fn copy_to_scratch(
    file: &File,
    temps: &TempFolders,
    cancelled: &dyn Fn() -> bool,
) -> io::Result<TempFolder> {
    let folder = temps.create()?;
    let path = folder.file_path();
    let mut copy = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)?;
    let mut buffer = vec![0_u8; 1 << 20];
    let mut from = file;
    loop {
        if cancelled() {
            return Err(io::Error::from(io::ErrorKind::Interrupted));
        }
        let n = from.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        copy.write_all(&buffer[..n])?;
    }
    drop(copy);
    fs::set_permissions(&path, fs::Permissions::from_mode(0o400))?;
    Ok(folder)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A kept file's name fits a volume's limit: the stem is shortened, at
    /// a character boundary, and the suffix and extension kept.
    #[test]
    fn kept_names_fit_name_max() {
        let short = kept_name(Path::new("/x/a.csv"), " (replaced, kept by Leal)");
        assert_eq!(short, "a (replaced, kept by Leal).csv");
        let long = format!("/x/{}.csv", "\u{e9}".repeat(200));
        let name = kept_name(Path::new(&long), " (replaced, kept by Leal 2)");
        assert!(name.len() <= NAME_MAX, "{}", name.len());
        assert!(name.len() > NAME_MAX - 2);
        assert!(name.ends_with("\u{e9} (replaced, kept by Leal 2).csv"));
        let odd = format!("/x/a.{}", "x".repeat(250));
        let name = kept_name(Path::new(&odd), " (replaced, kept by Leal)");
        assert_eq!(name, "a (replaced, kept by Leal)");
        assert_eq!(kept_name(Path::new("/x/a.csv"), ""), "a.csv");
    }
}
