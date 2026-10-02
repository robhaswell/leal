//! The scheduler's platform on macOS (DESIGN §3.10): thread quality of
//! service, the number of performance cores, and `os_signpost` intervals
//! for Instruments.
//!
//! These are C APIs the standard library and `libc` don't wrap, so this is
//! the one module of leal-ffi with `unsafe` code (CLAUDE.md). Each function
//! wraps one call in a safe signature, and each `unsafe` block says why it
//! is sound.
//!
//! **Signposts.** The `os_signpost_interval_begin` and `_end` macros of
//! `<os/signpost.h>` expand to a call to `_os_signpost_emit_with_name_impl`
//! with the image's `__dso_handle`, the interval's name and an encoded
//! format buffer. The system finds the name and format strings by their
//! offset in the binary, so, as the C macros do, they live in the
//! `__TEXT,__oslogstring` section. The intervals go to the subsystem
//! [`SUBSYSTEM`], category `PointsOfInterest`, so Instruments shows them in
//! the Points of Interest lane of every template, and in the os_signpost
//! instrument.

use std::ffi::{CStr, c_char, c_int, c_void};
use std::sync::OnceLock;

use leal_core::schedule::{Interval, Platform, ThreadClass};

/// The `os_log` subsystem Leal's intervals are logged under.
pub const SUBSYSTEM: &CStr = c"io.github.robhaswell.leal";

/// `PointsOfInterest`: Instruments shows this category's signposts in every
/// template.
const CATEGORY: &CStr = c"PointsOfInterest";

/// `qos_class_t` values from `<sys/qos.h>`.
const QOS_CLASS_USER_INITIATED: u32 = 0x19;
const QOS_CLASS_UTILITY: u32 = 0x11;

/// `os_signpost_type_t` values from `<os/signpost.h>`.
const OS_SIGNPOST_INTERVAL_BEGIN: u8 = 0x01;
const OS_SIGNPOST_INTERVAL_END: u8 = 0x02;

/// An `os_log_t`: an opaque object pointer.
#[repr(C)]
struct OsLog {
    _private: [u8; 0],
}

unsafe extern "C" {
    /// The Mach-O header of the image this code is linked into. The linker
    /// defines it; `os_signpost` uses it to find the strings below.
    static __dso_handle: u8;

    fn os_log_create(subsystem: *const c_char, category: *const c_char) -> *mut OsLog;
    fn os_signpost_enabled(log: *mut OsLog) -> bool;
    fn _os_signpost_emit_with_name_impl(
        dso: *const c_void,
        log: *mut OsLog,
        kind: u8,
        id: u64,
        name: *const c_char,
        format: *const c_char,
        buf: *mut u8,
        size: u32,
    );
    fn pthread_set_qos_class_self_np(qos_class: u32, relative_priority: c_int) -> c_int;
}

/// The signpost names and their (empty) format, where `os_log` expects
/// them, as `OS_LOG_STRING` puts them. Each is NUL-terminated.
macro_rules! log_strings {
    ($($name:ident = $text:literal;)*) => {
        $(
            #[unsafe(link_section = "__TEXT,__oslogstring,cstring_literals")]
            static $name: [u8; $text.len()] = *$text;
        )*
    };
}

log_strings! {
    FIRST_PAINT = b"First paint\0";
    INDEX = b"Index\0";
    REVIEW = b"Review\0";
    DIAGNOSTICS = b"Diagnostics\0";
    ACCELERATION = b"Acceleration\0";
    FIND = b"Find\0";
    COPY = b"Copy\0";
    PAUSED = b"Paused\0";
    EMPTY_FORMAT = b"\0";
}

/// The name string for an interval.
fn name(interval: Interval) -> &'static [u8] {
    match interval {
        Interval::FirstPaint => &FIRST_PAINT,
        Interval::Index => &INDEX,
        Interval::Review => &REVIEW,
        Interval::Diagnostics => &DIAGNOSTICS,
        Interval::Acceleration => &ACCELERATION,
        Interval::Find => &FIND,
        Interval::Copy => &COPY,
        Interval::Paused => &PAUSED,
    }
}

/// The app's platform: see the module docs.
#[derive(Debug, Default)]
pub struct MacPlatform;

impl Platform for MacPlatform {
    fn thread_started(&self, class: ThreadClass) {
        let qos = match class {
            ThreadClass::Index | ThreadClass::Watcher => QOS_CLASS_USER_INITIATED,
            ThreadClass::Background => QOS_CLASS_UTILITY,
        };
        set_thread_qos(qos);
    }

    fn performance_cores(&self) -> Option<usize> {
        performance_cores()
    }

    fn begin(&self, interval: Interval, id: u64) {
        emit(OS_SIGNPOST_INTERVAL_BEGIN, interval, id);
    }

    fn end(&self, interval: Interval, id: u64) {
        emit(OS_SIGNPOST_INTERVAL_END, interval, id);
    }
}

/// Sets this thread's quality of service. A failure (which only an invalid
/// class gives) leaves the thread at its default.
fn set_thread_qos(qos: u32) {
    // SAFETY: `pthread_set_qos_class_self_np` changes only the calling
    // thread's scheduling class, takes no pointers, and `qos` is one of the
    // valid `qos_class_t` values from <sys/qos.h>.
    let _ = unsafe { pthread_set_qos_class_self_np(qos, 0) };
}

/// The number of performance cores (`hw.perflevel0.physicalcpu`). `None`
/// if the system doesn't say, as on an Intel Mac.
pub fn performance_cores() -> Option<usize> {
    let mut cores: c_int = 0;
    let mut size = size_of::<c_int>();
    // SAFETY: the name is a NUL-terminated string; `cores` and `size` are
    // live, writable and the right size (`size` says how many bytes
    // `cores` holds, and `sysctlbyname` writes at most that many); no new
    // value is set (null pointer, length 0).
    let status = unsafe {
        libc::sysctlbyname(
            c"hw.perflevel0.physicalcpu".as_ptr(),
            (&raw mut cores).cast::<c_void>(),
            &raw mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    let cores = usize::try_from(cores).ok().filter(|&n| n > 0)?;
    (status == 0 && size == size_of::<c_int>()).then_some(cores)
}

/// The open-file limit the app asks for: `OPEN_MAX` from
/// `<sys/syslimits.h>`, the most macOS lets a process's soft
/// `RLIMIT_NOFILE` be, and what apps commonly raise it to.
pub const OPEN_FILES: u64 = 10_240;

/// Raises this process's soft limit on open files (`RLIMIT_NOFILE`) to
/// [`OPEN_FILES`], or to the hard limit if that is lower, and returns the
/// soft limit now. It never lowers it.
///
/// An app launched from the Finder gets a soft limit of 256. Each open
/// document holds 3 or 4 descriptors (its temporary record, its watcher's
/// event queue and the watched file, and the copy on a removable drive),
/// and AppKit holds its own, so a few dozen documents would run out
/// (p1-review conc-6): opening would fail, and watching a file would too.
///
/// # Errors
///
/// The `getrlimit` or `setrlimit` error. The limit is then as it was.
pub fn raise_open_file_limit() -> std::io::Result<u64> {
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: `limit` is a live, writable `rlimit`, the one argument
    // `getrlimit` writes to.
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &raw mut limit) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let wanted = OPEN_FILES.min(limit.rlim_max);
    if limit.rlim_cur >= wanted {
        return Ok(limit.rlim_cur);
    }
    let raised = libc::rlimit {
        rlim_cur: wanted,
        rlim_max: limit.rlim_max,
    };
    // SAFETY: `raised` is a live `rlimit`, which `setrlimit` only reads.
    if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &raw const raised) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(wanted)
}

/// The log, made once. Stored as an address so the `OnceLock` is `Sync`;
/// `os_log_t` objects are immutable and thread-safe.
fn log() -> *mut OsLog {
    static LOG: OnceLock<usize> = OnceLock::new();
    let address = *LOG.get_or_init(|| {
        // SAFETY: both arguments are NUL-terminated strings that live for
        // the whole program. The returned object is never released, so it
        // stays valid.
        let log = unsafe { os_log_create(SUBSYSTEM.as_ptr(), CATEGORY.as_ptr()) };
        log as usize
    });
    address as *mut OsLog
}

/// Emits one signpost, if anyone is recording.
fn emit(kind: u8, interval: Interval, id: u64) {
    // 0 and `!0` are OS_SIGNPOST_ID_NULL and OS_SIGNPOST_ID_INVALID.
    if id == 0 || id == u64::MAX {
        return;
    }
    let log = log();
    if log.is_null() {
        return;
    }
    // SAFETY: `log` is a live `os_log_t` from `os_log_create`.
    if !unsafe { os_signpost_enabled(log) } {
        return;
    }
    // What `__builtin_os_log_format` writes for a format with no
    // arguments: a summary byte and an argument count, both 0.
    let mut buf = [0u8; 2];
    // SAFETY: `__dso_handle` is the linker's symbol for this image's
    // header, and only its address is taken. `log` is live. The name and
    // format are NUL-terminated strings in `__TEXT,__oslogstring`, which
    // live for the whole program, as the C macros arrange. `buf` is a
    // writable buffer of `size` bytes, encoded as the format needs.
    unsafe {
        _os_signpost_emit_with_name_impl(
            (&raw const __dso_handle).cast::<c_void>(),
            log,
            kind,
            id,
            name(interval).as_ptr().cast::<c_char>(),
            EMPTY_FORMAT.as_ptr().cast::<c_char>(),
            buf.as_mut_ptr(),
            2,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_name_is_a_c_string() {
        for interval in [
            Interval::FirstPaint,
            Interval::Index,
            Interval::Review,
            Interval::Diagnostics,
            Interval::Acceleration,
            Interval::Find,
            Interval::Copy,
            Interval::Paused,
        ] {
            let name = CStr::from_bytes_with_nul(super::name(interval)).unwrap();
            assert_eq!(name.to_str().unwrap(), interval.name());
        }
        assert_eq!(CStr::from_bytes_with_nul(&EMPTY_FORMAT).unwrap(), c"");
    }

    #[test]
    fn performance_core_count_is_sane() {
        // CI runs in virtual Macs that report few or no performance cores,
        // so only check what holds everywhere: a reported count is never 0.
        // The scheduler falls back to the logical core count when this is None.
        let cores = performance_cores();
        assert!(cores.is_none_or(|n| n >= 1), "{cores:?}");
    }

    /// p1-review conc-6: the app's open-file limit is raised from the
    /// Finder's 256. It is never lowered.
    #[test]
    fn the_open_file_limit_is_raised() {
        let before = open_file_limit();
        let now = raise_open_file_limit().unwrap();
        let limit = open_file_limit();
        assert_eq!(limit.rlim_cur, now);
        assert!(now >= before.rlim_cur, "{now} < {}", before.rlim_cur);
        assert!(now >= OPEN_FILES.min(limit.rlim_max), "{now}");
        // Lowered to the Finder's limit (as a shell may have it already),
        // it is raised again.
        if limit.rlim_max >= OPEN_FILES {
            let finder = libc::rlimit {
                rlim_cur: 256,
                rlim_max: limit.rlim_max,
            };
            // SAFETY: `finder` is a live `rlimit`, which `setrlimit` only
            // reads.
            let set = unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &raw const finder) };
            assert_eq!(set, 0);
            assert_eq!(open_file_limit().rlim_cur, 256);
            assert_eq!(raise_open_file_limit().unwrap(), OPEN_FILES);
            assert_eq!(open_file_limit().rlim_cur, OPEN_FILES);
        }
    }

    fn open_file_limit() -> libc::rlimit {
        let mut limit = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        // SAFETY: `limit` is a live, writable `rlimit`, the one argument
        // `getrlimit` writes to.
        let got = unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &raw mut limit) };
        assert_eq!(got, 0);
        limit
    }

    #[test]
    fn intervals_and_qos_do_not_crash() {
        let platform = MacPlatform;
        std::thread::spawn(move || {
            platform.thread_started(ThreadClass::Background);
            platform.begin(Interval::Review, 7);
            platform.end(Interval::Review, 7);
            platform.begin(Interval::FirstPaint, 0);
        })
        .join()
        .unwrap();
    }
}
