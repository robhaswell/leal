//! The system calls [`super`] needs that the standard library doesn't wrap:
//! cloning a file, reading an extended attribute, clearing `O_NONBLOCK`, a
//! volume's mount flags, the memory map itself, whether this is the main
//! thread, and for watching the
//! user's file, a kernel event queue (`kqueue`) and an open file's current
//! path (`F_GETPATH`). (Ordinary reads at an
//! offset, `pread`, need no `unsafe`: the standard library has them as
//! `FileExt::read_at`.)
//!
//! This is the only module in `leal-core` allowed to use `unsafe`
//! (CLAUDE.md). Each function wraps exactly one unsafe operation in a safe
//! signature, with a `// SAFETY:` comment saying why the call is sound.

use std::ffi::{CStr, CString, OsString};
use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

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

/// Sets the BSD file flags (`chflags(2)`) of the file at `path` to `flags`.
///
/// `fclonefileat` copies the original's flags, so a clone of a Finder-locked
/// (`uchg`) or append-only (`uappnd`) file can't have its mode changed or be
/// deleted until they are cleared. See [`super::temp::unlock`].
pub(super) fn set_file_flags(path: &Path, flags: u32) -> io::Result<()> {
    let path = c_path(path)?;
    // SAFETY: `chflags` reads the NUL-terminated string `path`, which lives
    // until the end of this function, and doesn't keep the pointer. `flags`
    // is a plain integer.
    let result = unsafe { libc::chflags(path.as_ptr(), flags) };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
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
    //   alone, under a name no other program uses. Its locking flags are
    //   cleared and it is made read-only (mode 0400) before it is mapped;
    //   `open` fails if either step fails.
    // - Leal never writes it while it is mapped: `file` is opened
    //   read-only, and the map is read-only (`PROT_READ`). The internal copy
    //   of a file on a removable drive, which `Source::stream` writes
    //   (ADR-0006), is mapped only once the stream has written all of it
    //   and made it read-only, and nothing writes it after that.
    // - It is never on a volume that can vanish: if a removable drive is
    //   unplugged or force-ejected, the mapped pages go with it and reading
    //   an unloaded one kills the process with SIGBUS. So nothing on a
    //   removable volume is ever mapped (ADR-0006 option C): the clone there
    //   (or the user's file, if the volume can't clone) is read with
    //   `pread`, which returns an error instead, and copied to the internal
    //   disk, and that copy is what gets mapped. The fallback copy
    //   and the internal copy are in the scratch directory, on the boot
    //   volume.
    // Another process running as the same user could still deliberately
    // open and change it, as it could any of the user's files; we accept
    // that, as every macOS app that maps files does.
    unsafe { Mmap::map(file) }
}

/// The mount flags (`f_flags`, the `MNT_*` constants) of the volume the
/// open file `file` is on, and the name of its file system type
/// (`f_fstypename`, such as `apfs`, `smbfs` or `nfs`), from `fstatfs(2)`.
pub(super) fn volume_flags(file: &File) -> io::Result<(u32, String)> {
    let mut stats = std::mem::MaybeUninit::<libc::statfs>::uninit();
    // SAFETY: `fstatfs` writes one `struct statfs` to the pointer, which
    // points at uninitialised memory of exactly that type and size, owned by
    // this function. The descriptor is open and borrowed from `file` for
    // the length of the call.
    let result = unsafe { libc::fstatfs(file.as_raw_fd(), stats.as_mut_ptr()) };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fstatfs` returned 0, so it filled in the whole struct.
    let stats = unsafe { stats.assume_init() };
    // `f_fstypename` is a fixed array of C chars, NUL-terminated when the
    // name is shorter than the array. Read up to the NUL, or the whole
    // array, without trusting it to have one.
    let name: Vec<u8> = stats
        .f_fstypename
        .iter()
        .take_while(|&&c| c != 0)
        .map(|&c| c.to_ne_bytes()[0])
        .collect();
    Ok((stats.f_flags, String::from_utf8_lossy(&name).into_owned()))
}

/// Whether the calling thread is the process's main thread
/// (`pthread_main_np(3)`): the one AppKit draws on. Network shares are
/// never read from it (ADR-0009).
pub(super) fn is_main_thread() -> bool {
    // SAFETY: `pthread_main_np` takes no arguments, touches no memory of
    // ours, and only reports whether the calling thread is the initial one.
    unsafe { libc::pthread_main_np() == 1 }
}

/// The path the open file `file` has now, with `fcntl(F_GETPATH)`. It
/// follows renames: after the file is moved, it gives the new path. For a
/// file that has been deleted it may still give the old one.
pub(super) fn path_of(file: &File) -> io::Result<PathBuf> {
    let mut buffer = vec![0_u8; usize::try_from(libc::MAXPATHLEN).unwrap_or(1024)];
    // SAFETY: `F_GETPATH` writes a NUL-terminated path of at most
    // `MAXPATHLEN` bytes, including the NUL, into the buffer it is given;
    // `buffer` is exactly that long and owned by this function. The
    // descriptor is open and borrowed from `file` for the length of the
    // call.
    let result = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETPATH, buffer.as_mut_ptr()) };
    if result == -1 {
        return Err(io::Error::last_os_error());
    }
    let len = buffer
        .iter()
        .position(|&byte| byte == 0)
        .unwrap_or(buffer.len());
    buffer.truncate(len);
    Ok(PathBuf::from(OsString::from_vec(buffer)))
}

/// A kernel event queue (`kqueue(2)`), for watching the user's file
/// (task 1.9). The descriptor is closed when it is dropped.
pub(super) struct Kqueue(OwnedFd);

/// One event [`Kqueue::wait`] returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Event {
    /// The descriptor (for a file) or the number (for a user event) it is
    /// about.
    pub ident: usize,
    /// Whether it is a user event ([`Kqueue::trigger`]) rather than a
    /// file's.
    pub user: bool,
    /// For a file, what happened: `NOTE_WRITE`, `NOTE_DELETE` and so on.
    pub fflags: u32,
}

impl Kqueue {
    /// A new, empty queue.
    pub(super) fn new() -> io::Result<Self> {
        // SAFETY: `kqueue` takes no arguments and returns a new descriptor,
        // or -1.
        let fd = unsafe { libc::kqueue() };
        if fd == -1 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `fd` is a descriptor `kqueue` just opened, which nothing
        // else owns, so `OwnedFd` may close it.
        Ok(Self(unsafe { OwnedFd::from_raw_fd(fd) }))
    }

    /// Reports `fflags` (`NOTE_WRITE`, …) about `file` until `file` is
    /// closed, which removes it from the queue. Each kind is reported once
    /// however often it happens before [`wait`](Self::wait) collects it
    /// (`EV_CLEAR`).
    pub(super) fn watch(&self, file: &File, fflags: u32) -> io::Result<()> {
        self.change(libc::kevent {
            ident: usize::try_from(file.as_raw_fd()).unwrap_or(usize::MAX),
            filter: libc::EVFILT_VNODE,
            flags: libc::EV_ADD | libc::EV_CLEAR,
            fflags,
            data: 0,
            udata: std::ptr::null_mut(),
        })
    }

    /// Adds the user event `ident`, which [`trigger`](Self::trigger) fires.
    pub(super) fn add_user(&self, ident: usize) -> io::Result<()> {
        self.change(libc::kevent {
            ident,
            filter: libc::EVFILT_USER,
            flags: libc::EV_ADD | libc::EV_CLEAR,
            fflags: libc::NOTE_FFNOP,
            data: 0,
            udata: std::ptr::null_mut(),
        })
    }

    /// Fires the user event `ident`, waking a [`wait`](Self::wait).
    pub(super) fn trigger(&self, ident: usize) -> io::Result<()> {
        self.change(libc::kevent {
            ident,
            filter: libc::EVFILT_USER,
            flags: 0,
            fflags: libc::NOTE_TRIGGER,
            data: 0,
            udata: std::ptr::null_mut(),
        })
    }

    /// Registers one change with the kernel.
    fn change(&self, change: libc::kevent) -> io::Result<()> {
        // SAFETY: `kevent` reads one `struct kevent` from `&change`, which
        // lives until the end of this function, and writes no events (the
        // event list is null with a length of 0). A null timeout is not
        // used when there are no events to wait for. The queue's
        // descriptor is open and owned by `self`.
        let result = unsafe {
            libc::kevent(
                self.0.as_raw_fd(),
                &raw const change,
                1,
                std::ptr::null_mut(),
                0,
                std::ptr::null(),
            )
        };
        if result == -1 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    /// Waits for events, for at most `timeout` (`None`: as long as it
    /// takes), and returns them; none if the time ran out. A signal
    /// interrupting the wait (`EINTR`) also gives none.
    pub(super) fn wait(&self, timeout: Option<Duration>) -> io::Result<Vec<Event>> {
        const CAPACITY: usize = 16;
        let mut events: Vec<libc::kevent> = Vec::with_capacity(CAPACITY);
        let timeout = timeout.map(|timeout| libc::timespec {
            tv_sec: libc::time_t::try_from(timeout.as_secs()).unwrap_or(libc::time_t::MAX),
            tv_nsec: libc::c_long::from(timeout.subsec_nanos()),
        });
        let timeout_ptr = timeout
            .as_ref()
            .map_or(std::ptr::null(), std::ptr::from_ref);
        // SAFETY: `kevent` writes at most `CAPACITY` events into `events`'
        // spare capacity, which is that long, and returns how many it wrote
        // (or -1); no changes are passed. `timeout_ptr` is null or points at
        // `timeout`, which lives until the end of this function. The queue's
        // descriptor is open and owned by `self`.
        let count = unsafe {
            libc::kevent(
                self.0.as_raw_fd(),
                std::ptr::null(),
                0,
                events.as_mut_ptr(),
                libc::c_int::try_from(CAPACITY).unwrap_or(1),
                timeout_ptr,
            )
        };
        let Ok(count) = usize::try_from(count) else {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                return Ok(Vec::new());
            }
            return Err(error);
        };
        // SAFETY: `kevent` initialised the first `count` events, and
        // `count <= CAPACITY`, the vector's capacity.
        unsafe { events.set_len(count.min(CAPACITY)) };
        Ok(events
            .iter()
            .map(|event| Event {
                ident: event.ident,
                user: event.filter == libc::EVFILT_USER,
                fflags: event.fflags,
            })
            .collect())
    }
}

/// Swaps the files at `a` and `b` atomically (`renamex_np` with
/// `RENAME_SWAP`), as `FileManager.replaceItemAt` does for a safe save.
/// Tests only: they reproduce that save.
#[cfg(test)]
pub(super) fn swap(a: &Path, b: &Path) -> io::Result<()> {
    let a = c_path(a)?;
    let b = c_path(b)?;
    // SAFETY: `renamex_np` reads the two NUL-terminated strings, which live
    // until the end of this function, and doesn't keep the pointers.
    // `RENAME_SWAP` is a plain flag.
    let result = unsafe { libc::renamex_np(a.as_ptr(), b.as_ptr(), libc::RENAME_SWAP) };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// `path` as a NUL-terminated C string.
fn c_path(path: &Path) -> io::Result<CString> {
    CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains a NUL byte"))
}
