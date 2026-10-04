//! Whether a file's volume can vanish while it is open: a removable drive
//! (ADR-0006, PLAN 1.1a) or a network share (ADR-0009).
//!
//! Two sources of facts, either of which saying "it can vanish" is enough:
//!
//! - **Foundation's** `volumeIsInternal` and `volumeIsEjectable` resource
//!   values, which the app reads for the file and passes in
//!   ([`VolumeInfo`]). A volume can vanish unless it is known to be internal
//!   and isn't ejectable. These are the properties PLAN 1.1a names. A disk
//!   image, for example, has no "is internal" value but is ejectable.
//! - **The volume's mount flags** (`fstatfs`), which the core reads itself,
//!   so the CLI and Rust tests get a sensible answer without Foundation:
//!   `MNT_REMOVABLE` (removable media; disk images have it, which the tests
//!   check, but Apple doesn't document whether every external fixed disk
//!   does, which is why the app's facts come first).
//!
//! A volume that isn't local (`MNT_LOCAL` missing), or whose file system is
//! a network one by name (`f_fstypename`: `smbfs`, `nfs`, `afpfs`,
//! `webdav`, `ftp`), is a network share: [`VolumeKind::Network`]. Either is
//! enough, so a network file system that sets `MNT_LOCAL` anyway is still a
//! share. Shares take the removable-drive path too (ADR-0009), with their
//! own safety rules (`removable::ShareRules`).
//!
//! If the flags can't be read, the volume is assumed to be removable: the
//! removable path is always correct, only slower to map.

use std::fs::File;

use super::{VolumeInfo, sys};

/// `MNT_REMOVABLE` from `<sys/mount.h>`, which the `libc` crate doesn't
/// define.
const MNT_REMOVABLE: u32 = 0x0000_0200;

/// The `f_fstypename`s of macOS's network file systems: SMB (`mount_smbfs`),
/// NFS, AFP, WebDAV and FTP.
const NETWORK_FILE_SYSTEMS: [&str; 5] = ["smbfs", "nfs", "afpfs", "webdav", "ftp"];

/// The mount flags that matter here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct VolumeFlags {
    /// `MNT_LOCAL`: the volume is stored locally, not on a network.
    pub(super) local: bool,
    /// `MNT_REMOVABLE`: removable media, or a device on an external bus.
    pub(super) removable: bool,
    /// The file system type is a network one ([`NETWORK_FILE_SYSTEMS`]).
    pub(super) network_type: bool,
}

/// The mount flags of the volume `file` is on.
pub(super) fn flags(file: &File) -> std::io::Result<VolumeFlags> {
    let (flags, type_name) = sys::volume_flags(file)?;
    Ok(decode(flags, &type_name))
}

/// The mount flags of the volume the item at `path` is on, without
/// opening it (`statfs`): the sandbox lets the app look at a folder it may
/// not open.
pub(super) fn flags_at(path: &std::path::Path) -> std::io::Result<VolumeFlags> {
    let (flags, type_name) = sys::volume_flags_at(path)?;
    Ok(decode(flags, &type_name))
}

/// `statfs`'s mount flags and file system type name, as [`VolumeFlags`].
fn decode(flags: u32, type_name: &str) -> VolumeFlags {
    let local = u32::try_from(libc::MNT_LOCAL).unwrap_or(0);
    VolumeFlags {
        local: flags & local != 0,
        removable: flags & MNT_REMOVABLE != 0,
        network_type: is_network_file_system(type_name),
    }
}

/// Whether `type_name` (an `f_fstypename`) is a network file system.
pub(super) fn is_network_file_system(type_name: &str) -> bool {
    NETWORK_FILE_SYSTEMS
        .iter()
        .any(|name| type_name.eq_ignore_ascii_case(name))
}

/// What kind of volume a file is on, for choosing how to read it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum VolumeKind {
    /// Can't vanish while the file is open: an internal disk.
    Fixed,
    /// A removable drive (ADR-0006 option C): never mapped from there.
    Removable,
    /// A network share (ADR-0009): read like a removable drive, and also
    /// never read on the main thread past what is copied, with network
    /// errors retried and a vanished file reported as deleted.
    Network,
}

/// The kind of volume these facts and flags describe. `flags` is `None` if
/// they couldn't be read; then the volume is assumed to be removable.
pub(super) fn kind(info: &VolumeInfo, flags: Option<VolumeFlags>) -> VolumeKind {
    if flags.is_some_and(|flags| !flags.local || flags.network_type) {
        return VolumeKind::Network;
    }
    let facts_known = info.is_internal.is_some() || info.is_ejectable.is_some();
    let by_facts =
        facts_known && (info.is_internal != Some(true) || info.is_ejectable == Some(true));
    let by_flags = flags.is_none_or(|flags| flags.removable);
    if by_facts || by_flags {
        VolumeKind::Removable
    } else {
        VolumeKind::Fixed
    }
}
