//! The system calls [`super`] needs that the standard library doesn't wrap:
//! cloning a file, reading an extended attribute, clearing `O_NONBLOCK`, a
//! volume's mount flags, the memory map itself, whether this is the main
//! thread, and for watching the
//! user's file, a kernel event queue (`kqueue`) and an open file's current
//! path (`F_GETPATH`); and for the simulated share's delay (a test hook),
//! a kernel timer that isn't coalesced. For saving (task 2.2): copying a
//! file's access control list and extended attributes, setting and
//! removing an attribute, its flags and its creation date. (Ordinary reads at an
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

/// TEST HOOK: blocks the calling thread for `duration`, on a kernel timer
/// the system may not coalesce (`EVFILT_TIMER` with `NOTE_CRITICAL`), so
/// it wakes on time whatever the thread's QoS. `thread::sleep` doesn't: at
/// background QoS, or in a background process (`taskpolicy -b`; GitHub's
/// macOS runners behave like one, docs/tasks/2.0.md), the kernel may defer
/// its wake-up by up to 32 times the sleep, capped at 100 ms, so a 20 ms
/// sleep takes up to 120 ms. A simulated share's round trip must take its
/// delay, as a real one does: a network read wakes when the reply arrives,
/// not on a coalesced timer.
#[cfg(any(test, feature = "test-hooks"))]
pub(super) fn sleep_strictly(duration: Duration) -> io::Result<()> {
    let queue = Kqueue::new()?;
    queue.change(libc::kevent {
        ident: 0,
        filter: libc::EVFILT_TIMER,
        flags: libc::EV_ADD | libc::EV_ONESHOT,
        fflags: libc::NOTE_NSECONDS | libc::NOTE_CRITICAL,
        data: isize::try_from(duration.as_nanos()).unwrap_or(isize::MAX),
        udata: std::ptr::null_mut(),
    })?;
    // The timer is the queue's only event; none means a signal
    // interrupted the wait, and the timer is still set.
    while queue.wait(None)?.is_empty() {}
    Ok(())
}

/// Swaps the files at `a` and `b` atomically (`renamex_np` with
/// `RENAME_SWAP`), as `FileManager.replaceItemAt` does for a safe save, and
/// as Leal's own save does (task 2.2). Fails with `ENOTSUP` (or `EINVAL`)
/// on a volume that can't swap, such as some FAT and SMB volumes, and
/// `ENOENT` if either is missing.
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

unsafe extern "C" {
    /// `acl_get_fd_np(3)`: the open file's access control list, or null
    /// (`ENOENT` if it has none). The caller frees it with `acl_free`.
    fn acl_get_fd_np(fd: libc::c_int, kind: libc::c_uint) -> *mut libc::c_void;
    /// `acl_set_fd_np(3)`: sets the open file's access control list.
    fn acl_set_fd_np(fd: libc::c_int, acl: *mut libc::c_void, kind: libc::c_uint) -> libc::c_int;
    /// `acl_free(3)`.
    fn acl_free(object: *mut libc::c_void) -> libc::c_int;
    /// `acl_init(3)`: a new, empty access control list, or null.
    fn acl_init(count: libc::c_int) -> *mut libc::c_void;
}

/// `ACL_TYPE_EXTENDED`, the only kind macOS file systems have.
const ACL_TYPE_EXTENDED: libc::c_uint = 0x100;

/// An access control list from `acl_get_fd_np` or `acl_init`, freed when
/// dropped.
struct Acl(*mut libc::c_void);

impl Drop for Acl {
    fn drop(&mut self) {
        // SAFETY: the pointer came from `acl_get_fd_np` or `acl_init`, which
        // allocated it, is not null, and is freed only here, once.
        unsafe {
            acl_free(self.0);
        }
    }
}

/// Copies the access control list of the open file `from` to the open
/// file `to` (`acl_get_fd_np` and `acl_set_fd_np`), and nothing else: not
/// with `fcopyfile(COPYFILE_ACL)`, which also applies the old file's
/// quarantine, which a safe save doesn't keep. A save sets it last, so an
/// entry denying attribute or permission writes can't stop anything before
/// it (ADR-0012 decision 1). If `from` has none, `to` is given an empty one,
/// so entries it inherited from the folder it was made in don't stay; that
/// is best-effort, and not reported if it fails, as there was nothing of the
/// old file's to keep.
pub(super) fn copy_acl(from: &File, to: &File) -> io::Result<()> {
    // SAFETY: the descriptor is open, borrowed from `from` for the call;
    // the type is a plain integer. The result is null or a list this
    // function owns, which `Acl` frees.
    let acl = unsafe { acl_get_fd_np(from.as_raw_fd(), ACL_TYPE_EXTENDED) };
    if acl.is_null() {
        let error = io::Error::last_os_error();
        return match error.raw_os_error() {
            // It has none (or its volume has none): nothing of the old
            // file's to keep, so nothing to report if the clearing fails.
            Some(libc::ENOENT | libc::ENOTSUP) => {
                let _ = clear_acl(to);
                Ok(())
            }
            _ => Err(error),
        };
    }
    let acl = Acl(acl);
    // SAFETY: `acl.0` is a valid list from `acl_get_fd_np`, alive until
    // `acl` drops after this call, which only reads it. The descriptor is
    // open, borrowed from `to`.
    let result = unsafe { acl_set_fd_np(to.as_raw_fd(), acl.0, ACL_TYPE_EXTENDED) };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Gives the open file an empty access control list: entries a save had
/// copied are cleared before its metadata is copied again.
pub(super) fn clear_acl(file: &File) -> io::Result<()> {
    // SAFETY: `acl_init` takes a plain count and returns a new list this
    // function owns, or null.
    let empty = unsafe { acl_init(1) };
    if empty.is_null() {
        return Err(io::Error::last_os_error());
    }
    let empty = Acl(empty);
    // SAFETY: `empty.0` is a valid list, alive until `empty` drops after
    // this call, which only reads it. The descriptor is open, borrowed from
    // `file`.
    let result = unsafe { acl_set_fd_np(file.as_raw_fd(), empty.0, ACL_TYPE_EXTENDED) };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Sets the extended attribute `name` of the open file `file` to `value`,
/// with `fsetxattr(2)`, replacing any value it had.
pub(super) fn set_xattr(file: &File, name: &CStr, value: &[u8]) -> io::Result<()> {
    // SAFETY: `value` is valid for reads of `value.len()` bytes and `name`
    // is a NUL-terminated string; both outlive the call, which keeps no
    // pointer to them. The descriptor is open and borrowed from `file`.
    // Position 0 and options 0 set the whole value of an ordinary attribute.
    let result = unsafe {
        libc::fsetxattr(
            file.as_raw_fd(),
            name.as_ptr(),
            value.as_ptr().cast(),
            value.len(),
            0,
            0,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Removes the extended attribute `name` from the open file `file`, with
/// `fremovexattr(2)`. A file without it is left as it is.
pub(super) fn remove_xattr(file: &File, name: &CStr) -> io::Result<()> {
    // SAFETY: `name` is a NUL-terminated string that outlives the call,
    // which keeps no pointer to it. The descriptor is open and borrowed from
    // `file`. Options 0: an ordinary attribute.
    let result = unsafe { libc::fremovexattr(file.as_raw_fd(), name.as_ptr(), 0) };
    if result == 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ENOATTR) {
        return Ok(());
    }
    Err(error)
}

/// Sets the BSD file flags (`fchflags(2)`) of the open file `file`.
pub(super) fn set_flags_of(file: &File, flags: u32) -> io::Result<()> {
    // SAFETY: the descriptor is open and borrowed from `file` for the call;
    // `flags` is a plain integer.
    let result = unsafe { libc::fchflags(file.as_raw_fd(), flags) };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Sets the creation date (`ATTR_CMN_CRTIME`, what Finder shows as
/// Created) of the open file `file`, with `fsetattrlist(2)`.
pub(super) fn set_creation_time(file: &File, created: std::time::SystemTime) -> io::Result<()> {
    let since = created
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    let mut time = libc::timespec {
        tv_sec: libc::time_t::try_from(since.as_secs())
            .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?,
        tv_nsec: libc::c_long::from(since.subsec_nanos()),
    };
    let mut attributes = libc::attrlist {
        bitmapcount: libc::ATTR_BIT_MAP_COUNT,
        reserved: 0,
        commonattr: libc::ATTR_CMN_CRTIME,
        volattr: 0,
        dirattr: 0,
        fileattr: 0,
        forkattr: 0,
    };
    // SAFETY: `attributes` asks for one attribute, the creation time, whose
    // value in the buffer is one `struct timespec`; the buffer is `time`,
    // exactly that size, owned by this function and outliving the call,
    // which keeps no pointer to either. The descriptor is open and borrowed
    // from `file`. Options 0.
    let result = unsafe {
        libc::fsetattrlist(
            file.as_raw_fd(),
            (&raw mut attributes).cast(),
            (&raw mut time).cast(),
            std::mem::size_of::<libc::timespec>(),
            0,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Renames `from` to `to`, failing with `EEXIST` if something is at `to`
/// already (`renamex_np` with `RENAME_EXCL`): a Save As to a new place never
/// replaces a file that appeared there meanwhile.
pub(super) fn rename_new(from: &Path, to: &Path) -> io::Result<()> {
    let from = c_path(from)?;
    let to = c_path(to)?;
    // SAFETY: `renamex_np` reads the two NUL-terminated strings, which live
    // until the end of this function, and doesn't keep the pointers.
    // `RENAME_EXCL` is a plain flag.
    let result = unsafe { libc::renamex_np(from.as_ptr(), to.as_ptr(), libc::RENAME_EXCL) };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Whether this process may write the file at `path`, by its effective
/// user and groups and the file's mode and access control list
/// (`faccessat(2)` with `W_OK` and `AT_EACCESS`). A rename over a file
/// needs only write access to its folder, so a save asks this first.
pub(super) fn can_write(path: &Path) -> io::Result<bool> {
    let path = c_path(path)?;
    // SAFETY: `faccessat` reads the NUL-terminated string `path`, which
    // lives until the end of this function, and keeps no pointer to it.
    // `AT_FDCWD` only matters for a relative path. The mode and flags are
    // plain integers.
    let result =
        unsafe { libc::faccessat(libc::AT_FDCWD, path.as_ptr(), libc::W_OK, libc::AT_EACCESS) };
    if result == 0 {
        return Ok(true);
    }
    let error = io::Error::last_os_error();
    match error.raw_os_error() {
        Some(libc::EACCES | libc::EPERM | libc::EROFS) => Ok(false),
        _ => Err(error),
    }
}

/// Orders the open file's writes before any later ones on its volume
/// (`fcntl(F_BARRIERFSYNC)`): the new file's bytes are on the disk before
/// the rename that makes them the file. Where the volume doesn't support
/// barriers (`ENOTSUP`, `EINVAL`, `ENOTTY`), a full flush
/// (`F_FULLFSYNC`), and where that isn't supported either, `fsync(2)`.
pub(super) fn barrier_sync(file: &File) -> io::Result<()> {
    let unsupported = |error: &io::Error| {
        matches!(
            error.raw_os_error(),
            Some(libc::ENOTSUP | libc::EINVAL | libc::ENOTTY)
        )
    };
    // SAFETY: `F_BARRIERFSYNC` takes no argument. The descriptor is open and
    // borrowed from `file` for the length of the call.
    let result = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_BARRIERFSYNC) };
    if result != -1 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    if !unsupported(&error) {
        return Err(error);
    }
    // SAFETY: as above, for `F_FULLFSYNC`.
    let result = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_FULLFSYNC) };
    if result != -1 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    if !unsupported(&error) {
        return Err(error);
    }
    // SAFETY: `fsync` takes only the descriptor, open and borrowed from
    // `file`.
    let result = unsafe { libc::fsync(file.as_raw_fd()) };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// What the volume a folder is on can do for a save's rename
/// (`getattrlist(ATTR_VOL_CAPABILITIES)`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct RenameCapabilities {
    /// `renamex_np(RENAME_SWAP)` is supported (`VOL_CAP_INT_RENAME_SWAP`).
    /// APFS: yes; HFS+, exFAT and FAT: no. (FAT's driver returns success for
    /// a swap and does a plain rename, so the capability, not the result,
    /// decides.)
    pub(super) swap: bool,
    /// `renamex_np(RENAME_EXCL)` is supported (`VOL_CAP_INT_RENAME_EXCL`).
    /// APFS and HFS+: yes; exFAT and FAT: no.
    pub(super) exclusive: bool,
}

/// [`RenameCapabilities`] of the volume `folder` is on. Unknown bits count
/// as unsupported.
pub(super) fn rename_capabilities(folder: &Path) -> io::Result<RenameCapabilities> {
    #[repr(C)]
    struct Buffer {
        length: u32,
        capabilities: libc::vol_capabilities_attr_t,
    }
    let folder = c_path(folder)?;
    let mut attributes = libc::attrlist {
        bitmapcount: libc::ATTR_BIT_MAP_COUNT,
        reserved: 0,
        commonattr: 0,
        volattr: libc::ATTR_VOL_INFO | libc::ATTR_VOL_CAPABILITIES,
        dirattr: 0,
        fileattr: 0,
        forkattr: 0,
    };
    let mut buffer = std::mem::MaybeUninit::<Buffer>::zeroed();
    // SAFETY: `attributes` asks for the volume's capabilities only, which
    // `getattrlist` writes after a `u32` length: exactly `Buffer`'s layout
    // (`repr(C)`), which is zeroed, owned by this function, and as long as
    // the size passed. The path is NUL-terminated and outlives the call;
    // nothing keeps a pointer. Options 0 follow a symbolic link, as the
    // folder of a resolved destination has none.
    let result = unsafe {
        libc::getattrlist(
            folder.as_ptr(),
            (&raw mut attributes).cast(),
            buffer.as_mut_ptr().cast(),
            std::mem::size_of::<Buffer>(),
            0,
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: zeroed memory is a valid `Buffer` (plain integers), and
    // `getattrlist` filled in what it returned.
    let buffer = unsafe { buffer.assume_init() };
    let interfaces = libc::VOL_CAPABILITIES_INTERFACES;
    let has = |bit: u32| {
        buffer.capabilities.capabilities[interfaces] & bit != 0
            && buffer.capabilities.valid[interfaces] & bit != 0
    };
    Ok(RenameCapabilities {
        swap: has(libc::VOL_CAP_INT_RENAME_SWAP),
        exclusive: has(libc::VOL_CAP_INT_RENAME_EXCL),
    })
}

/// The names of the open file's extended attributes (`flistxattr(2)`).
pub(super) fn list_xattrs(file: &File) -> io::Result<Vec<CString>> {
    // SAFETY: a null buffer and size 0 ask only for the size the list
    // needs; nothing is written. The descriptor is open and borrowed from
    // `file`. Options 0.
    let size = unsafe { libc::flistxattr(file.as_raw_fd(), std::ptr::null_mut(), 0, 0) };
    let Ok(size) = usize::try_from(size) else {
        return Err(io::Error::last_os_error());
    };
    let mut names = vec![0_u8; size];
    // SAFETY: `names` is valid for writes of `names.len()` bytes, and
    // `flistxattr` writes at most that many. The descriptor is open.
    let len =
        unsafe { libc::flistxattr(file.as_raw_fd(), names.as_mut_ptr().cast(), names.len(), 0) };
    let Ok(len) = usize::try_from(len) else {
        return Err(io::Error::last_os_error());
    };
    names.truncate(len);
    Ok(names
        .split(|&b| b == 0)
        .filter(|name| !name.is_empty())
        .filter_map(|name| CString::new(name).ok())
        .collect())
}

/// The whole value of the open file's extended attribute `name`, however
/// long (a resource fork can be megabytes): `None` if it has none.
pub(super) fn read_whole_xattr(file: &File, name: &CStr) -> io::Result<Option<Vec<u8>>> {
    // SAFETY: a null buffer and size 0 ask only for the value's size. The
    // descriptor is open and borrowed from `file`; `name` is NUL-terminated
    // and outlives the call. Position 0, options 0.
    let size = unsafe {
        libc::fgetxattr(
            file.as_raw_fd(),
            name.as_ptr(),
            std::ptr::null_mut(),
            0,
            0,
            0,
        )
    };
    let Ok(size) = usize::try_from(size) else {
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ENOATTR) {
            return Ok(None);
        }
        return Err(error);
    };
    read_xattr(file, name, size)
}

unsafe extern "C" {
    /// `xattr_preserve_for_intent(3)`, from `<xattr_flags.h>`, in
    /// libSystem: whether an extended attribute should be kept for an
    /// operation, by the flags in its name or the system's defaults.
    fn xattr_preserve_for_intent(name: *const libc::c_char, intent: libc::c_uint) -> libc::c_int;
}

/// `XATTR_OPERATION_INTENT_SAVE`: a safe save, the new file replacing the
/// old.
const XATTR_OPERATION_INTENT_SAVE: libc::c_uint = 2;

/// Whether the system says the extended attribute `name` belongs on the
/// new version of a file a safe save writes (`xattr_preserve_for_intent`
/// with `XATTR_OPERATION_INTENT_SAVE`). Attributes tied to the contents
/// (`#C` in their name, such as a checksum) and ones never to be kept are
/// not.
pub(super) fn keep_on_save(name: &CStr) -> bool {
    // SAFETY: `xattr_preserve_for_intent` reads the NUL-terminated string
    // `name`, which outlives the call, and keeps no pointer to it. The
    // intent is a plain integer.
    unsafe { xattr_preserve_for_intent(name.as_ptr(), XATTR_OPERATION_INTENT_SAVE) != 0 }
}

/// `path` as a NUL-terminated C string.
fn c_path(path: &Path) -> io::Result<CString> {
    CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains a NUL byte"))
}
