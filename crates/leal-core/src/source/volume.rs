//! Whether a file's volume can vanish while it is open: a removable drive
//! (ADR-0006, PLAN 1.1a).
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
//! A volume that isn't local (`MNT_LOCAL` missing) is a network share. It
//! can vanish too, but ADR-0006 leaves it on the 1.1 fallbacks, so it gets
//! its own [`VolumeKind::Network`].
//!
//! If the flags can't be read, the volume is assumed to be removable: the
//! removable path is always correct, only slower to map.

use std::fs::File;

use super::{VolumeInfo, sys};

/// `MNT_REMOVABLE` from `<sys/mount.h>`, which the `libc` crate doesn't
/// define.
const MNT_REMOVABLE: u32 = 0x0000_0200;

/// The mount flags that matter here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct VolumeFlags {
    /// `MNT_LOCAL`: the volume is stored locally, not on a network.
    pub(super) local: bool,
    /// `MNT_REMOVABLE`: removable media, or a device on an external bus.
    pub(super) removable: bool,
}

/// The mount flags of the volume `file` is on.
pub(super) fn flags(file: &File) -> std::io::Result<VolumeFlags> {
    let flags = sys::volume_flags(file)?;
    let local = u32::try_from(libc::MNT_LOCAL).unwrap_or(0);
    Ok(VolumeFlags {
        local: flags & local != 0,
        removable: flags & MNT_REMOVABLE != 0,
    })
}

/// What kind of volume a file is on, for choosing how to read it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum VolumeKind {
    /// Can't vanish while the file is open: an internal disk.
    Fixed,
    /// A removable drive (ADR-0006 option C): never mapped from there.
    Removable,
    /// A network share (`MNT_LOCAL` missing). It can vanish too, but
    /// ADR-0006 keeps it on the 1.1 fallbacks (it can't clone, so it is
    /// read into memory or copied at open, which is safe). Streaming it
    /// like a removable drive is a proposal for Rob; see
    /// `docs/tasks/1.1a.md`.
    Network,
}

/// The kind of volume these facts and flags describe. `flags` is `None` if
/// they couldn't be read; then the volume is assumed to be removable.
pub(super) fn kind(info: &VolumeInfo, flags: Option<VolumeFlags>) -> VolumeKind {
    if flags.is_some_and(|flags| !flags.local) {
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
