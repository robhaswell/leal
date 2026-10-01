//! The system calls [`super`] needs that the standard library doesn't wrap:
//! cloning a file, reading an extended attribute, clearing `O_NONBLOCK`, and
//! the memory map itself.
//!
//! This is the only module in `leal-core` allowed to use `unsafe`
//! (CLAUDE.md). Each function wraps exactly one unsafe operation in a safe
//! signature, with a `// SAFETY:` comment saying why the call is sound.

use std::ffi::{CStr, CString};
use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use memmap2::Mmap;

/// Clones the open file `src` to a new file at `dest`, with
/// `fclonefileat(2)`. `dest` must not exist yet.
///
/// The clone shares its data blocks with `src` until either is written
/// (copy-on-write), so it takes no extra space and is nearly instant. It is
/// taken from the open file, so it is a snapshot of exactly the file that
/// was opened and checked, even if the path has been replaced since.
///
/// Fails with `EXDEV` if `dest` is on a different volume from `src`, and
/// with `ENOTSUP` if the volume can't clone (HFS+, exFAT, network shares).
pub(super) fn clone_file(src: &File, dest: &Path) -> io::Result<()> {
    let dest = c_path(dest)?;
    // SAFETY: `fclonefileat` reads the NUL-terminated string `dest`, which
    // lives until the end of this function, and doesn't keep the pointer.
    // `src.as_raw_fd()` is an open descriptor, borrowed from `src` for the
    // length of the call. `AT_FDCWD` only matters for a relative `dest`; ours
    // are absolute. Flags 0: no special behaviour.
    let result = unsafe { libc::fclonefileat(src.as_raw_fd(), libc::AT_FDCWD, dest.as_ptr(), 0) };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Reads the extended attribute `name` of the open file `file`, with
/// `fgetxattr(2)`.
///
/// Returns `Ok(None)` if the file has no such attribute, and an `ERANGE`
/// error if the value is longer than `max_len` bytes.
pub(super) fn read_xattr(file: &File, name: &CStr, max_len: usize) -> io::Result<Option<Vec<u8>>> {
    let mut value = vec![0_u8; max_len];
    // SAFETY: `value` is valid for writes of `value.len()` bytes, and
    // `fgetxattr` writes at most `size` bytes into it. `name` is a
    // NUL-terminated string that outlives the call. The descriptor is open
    // and borrowed from `file` for the length of the call. Position 0 and
    // options 0 read the whole value of an ordinary attribute.
    let len = unsafe {
        libc::fgetxattr(
            file.as_raw_fd(),
            name.as_ptr(),
            value.as_mut_ptr().cast(),
            value.len(),
            0,
            0,
        )
    };
    // A negative length means an error; `try_from` fails exactly then.
    let Ok(len) = usize::try_from(len) else {
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ENOATTR) {
            return Ok(None);
        }
        return Err(error);
    };
    value.truncate(len);
    Ok(Some(value))
}

/// Reads the open file's status flags (`fcntl(F_GETFL)`).
fn status_flags(file: &File) -> io::Result<libc::c_int> {
    // SAFETY: `F_GETFL` takes no argument and only reads the descriptor's
    // flags. The descriptor is open and borrowed from `file`.
    let flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFL) };
    if flags == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(flags)
    }
}

/// Clears `O_NONBLOCK` on an open file.
///
/// [`super::open_regular`] opens with `O_NONBLOCK` so that opening a FIFO
/// (named pipe) can't hang waiting for a writer. Once the file is known to
/// be a regular file the flag is cleared, so reads behave normally.
pub(super) fn clear_nonblocking(file: &File) -> io::Result<()> {
    let flags = status_flags(file)?;
    // SAFETY: `F_SETFL` takes one `int` argument, the new status flags,
    // which are the current ones without `O_NONBLOCK`. The descriptor is
    // open and borrowed from `file`.
    let result = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETFL, flags & !libc::O_NONBLOCK) };
    if result == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Maps `file` into memory, read-only.
///
/// Call it only on Leal's own clone or copy of the file the user opened
/// (see [`super::Source`]), never on the user's file itself.
pub(super) fn map_read_only(file: &File) -> io::Result<Mmap> {
    // SAFETY: a memory map is only sound while nothing changes the mapped
    // file: a write would change bytes that Rust treats as immutable, and a
    // truncation makes reading the missing pages crash the process
    // (SIGBUS). The kernel can't enforce that, so this relies on which file
    // is mapped:
    // - It is Leal's private clone (or copy) of the user's file, never the
    //   user's file. A clone is a separate file: when another program
    //   writes or truncates the original, APFS gives the original new
    //   blocks and the clone keeps the old ones (copy-on-write).
    // - It lives in a temporary folder that Leal made or was given for it
    //   alone, under a name no other program uses, and the file is made
    //   read-only (mode 0400) before it is mapped.
    // - Leal never writes it: `file` is opened read-only, and the map is
    //   read-only (`PROT_READ`).
    // Another process running as the same user could still deliberately
    // open and change it, as it could any of the user's files; we accept
    // that, as every macOS app that maps files does.
    unsafe { Mmap::map(file) }
}

/// `path` as a NUL-terminated C string.
fn c_path(path: &Path) -> io::Result<CString> {
    CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains a NUL byte"))
}
