//! Watching the user's file after it is opened (DESIGN §3.1, task 1.9).
//!
//! Leal works from a snapshot (a clone, or a copy), so nothing another
//! program does to the user's file changes what Leal shows. But the user
//! should know when the file they are looking at has changed, been moved or
//! deleted, and Save (phase 2) must not write over a file that changed
//! elsewhere without asking. [`Original`] keeps track of that:
//!
//! - It holds the file open for **event notifications only** (`O_EVTONLY`),
//!   which doesn't stop its volume from being ejected, and asks a kernel
//!   event queue (`kqueue`, the mechanism a dispatch source wraps) to report
//!   writes, growth, deletion, renames, attribute changes and the volume
//!   going away. [`Original::watch`] runs a thread that waits on the queue.
//! - After each event, and on [`Original::check`], it looks at the file
//!   (`fstat`) and compares its inode, size and modification time with the
//!   [`FileIdentity`] taken when it was opened.
//!
//! What each change becomes ([`OriginalState`]):
//!
//! - **Written, truncated or grown in place**: `Changed`. A write event
//!   counts even when the size and modification time look the same, which
//!   happens when a same-size write lands within the volume's
//!   modification-time granularity (1 s on HFS+, 2 s on FAT, 10 ms on
//!   exFAT, 1 ns on APFS).
//! - **Replaced** (an app saves by writing a new file and renaming it over
//!   the old one, so the old one is unlinked): `Changed`, and the new file
//!   at the path is watched from then on.
//! - **Renamed, with another file at its old path** (a safe save that swaps
//!   the new file in, `FileManager.replaceItemAt` or `renamex_np` with
//!   `RENAME_SWAP`, or that renames the old file to a backup and writes a
//!   new one, as Emacs does, keeping the backup or not): `Changed`, the
//!   same as a replacement, and the file now at the path is watched.
//! - **Moved** (renamed on the same volume) with nothing left at its old
//!   path: followed, but only once the move has stood for
//!   [`MOVE_WINDOW`] (2 s). Until then the move is *pending*: the status
//!   keeps the old path, and the old path is looked at every
//!   [`PENDING_POLL`] (100 ms), on the watching thread and in every
//!   [`Original::check`]. A regular file appearing there in the window
//!   makes it the backup step of a save, so a replacement (`Changed`), and
//!   the path the file was renamed to is never reported. After the window,
//!   the kernel's new path (`F_GETPATH`) is taken, the state stays
//!   `Unchanged`, and the old path is forgotten: a file that appears there
//!   later is unrelated. Moved into the Trash counts as `Deleted` (once
//!   the window has passed), and back out of it as before. A move into a
//!   temporary-items folder (`TemporaryItems`, where Foundation's safe
//!   saves stage files) is never followed: the file is passing through.
//! - **Deleted** (unlinked, with nothing at its path): `Deleted`. While
//!   deleted, the path is looked at once a second, so a file that appears
//!   there again (some tools delete and then write) becomes `Changed`.
//! - **Attributes only** (permissions, extended attributes, flags): no
//!   change, since the inode, size and modification time are the same.
//!   (Touching the modification time is a change: Leal can't tell that the
//!   contents are the same.)
//! - **Its volume unmounted** (a drive ejected or unplugged): `Unavailable`.
//!   Nothing can be watched until the volume is back. The app calls
//!   [`Original::check`] when a volume mounts, which looks for the file at
//!   its path again and compares it with what was opened, without the
//!   volume's device number, which a remount changes.
//!
//! **The limit.** While the file's volume is away there are no events, so
//! the comparison is all there is: a change elsewhere that keeps the size
//! and lands within the volume's modification-time granularity, or that
//! sets the modification time back, can't be seen. The same is true of any
//! change while Leal isn't running, which doesn't matter here.

use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use super::FileIdentity;
use super::sys::{self, Kqueue};

/// What has happened to the user's file since it was opened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OriginalState {
    /// It is as it was when opened: where it was, or where it was moved to.
    Unchanged,
    /// Its contents changed, or another file replaced it at its path. Leal
    /// still shows the snapshot it took; the app offers **Reload** and
    /// **Keep editing** (DESIGN §3.1).
    Changed,
    /// It was deleted, or moved to the Trash.
    Deleted,
    /// Its volume isn't mounted: a removable drive that was ejected or
    /// unplugged. Save is refused until it is back (ADR-0006).
    Unavailable,
}

/// The user's file as last seen, from [`Original::status`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OriginalStatus {
    /// What has happened to it.
    pub state: OriginalState,
    /// Where it is now: where it was opened from, or where it was moved to.
    pub path: PathBuf,
    /// Whether it has changed (or been deleted) at any time since it was
    /// opened. Once `true`, it stays `true`, even if the user chooses
    /// **Keep editing**: Save (phase 2) uses it to ask before writing over
    /// a file that changed elsewhere.
    pub diverged: bool,
    /// Whether the file that was opened, the same inode, was written to, or
    /// its size or modification time changed. For a file read without a
    /// snapshot (a removable drive that can't clone), that makes what is
    /// still to be read a different version ([`super::Source::note_original_written`]).
    pub(crate) written: bool,
}

/// The events the queue reports about the watched file.
const EVENTS: u32 = libc::NOTE_WRITE
    | libc::NOTE_EXTEND
    | libc::NOTE_DELETE
    | libc::NOTE_RENAME
    | libc::NOTE_ATTRIB
    | libc::NOTE_LINK
    | libc::NOTE_REVOKE;

/// The events that mean the watched file's contents were written.
const WRITES: u32 = libc::NOTE_WRITE | libc::NOTE_EXTEND;

/// The user event that stops the watching thread.
const STOP: usize = 1;

/// How often a deleted file's path is looked at, in case a file appears
/// there again.
const DELETED_POLL: Duration = Duration::from_secs(1);

/// How long a move must stand before it is followed. Within it, a file
/// appearing at the old path makes the move a save's backup step (see the
/// module docs). Safe saves take milliseconds, so 2 s leaves room for a
/// slow disk without making a real move wait long to show.
pub const MOVE_WINDOW: Duration = Duration::from_secs(2);

/// How often the old path of a pending move is looked at.
pub const PENDING_POLL: Duration = Duration::from_millis(100);

/// The user's file, watched (see the module docs).
pub struct Original {
    shared: Arc<Shared>,
    /// The watching thread, once [`watch`](Self::watch) has started it.
    thread: Mutex<Option<JoinHandle<()>>>,
}

struct Shared {
    /// The file as it was opened.
    opened: FileIdentity,
    /// `None` if the kernel wouldn't make a queue: then nothing is watched,
    /// and only [`Original::check`] notices changes.
    queue: Option<Kqueue>,
    /// The state, held while the file is looked at (system calls that can
    /// block on a slow volume).
    inner: Mutex<Inner>,
    /// The latest status, copied out of `inner` after each look. Its lock is
    /// held only to copy it, so [`Original::status`] never waits for a look
    /// (the main thread calls it, through `Document::can_save`).
    published: Mutex<OriginalStatus>,
}

struct Inner {
    /// Where the file is, as reported: during a pending move, still where
    /// it was.
    path: PathBuf,
    /// A move not followed yet, because it may be a save's backup step (see
    /// the module docs).
    pending: Option<PendingMove>,
    clock: Clock,
    /// The file being watched: the one that was opened, or the one that
    /// replaced it at its path. `None` while it is deleted or its volume is
    /// away, or if it couldn't be opened for events.
    watched: Option<Watched>,
    presence: Presence,
    diverged: bool,
    written: bool,
}

/// A move of the watched file that is still within [`MOVE_WINDOW`].
struct PendingMove {
    /// Where the kernel says the file is now.
    to: PathBuf,
    /// When the move was seen.
    since: Instant,
}

/// The time: the system's, or in tests one that stands still until the
/// test moves it (as `schedule::input`'s clock does).
#[derive(Default)]
struct Clock {
    #[cfg(test)]
    manual: Option<Instant>,
}

impl Clock {
    #[cfg_attr(
        not(test),
        expect(clippy::unused_self, reason = "only tests have a stopped clock")
    )]
    fn now(&self) -> Instant {
        #[cfg(test)]
        if let Some(now) = self.manual {
            return now;
        }
        Instant::now()
    }
}

struct Watched {
    /// Open for event notifications only (`O_EVTONLY`).
    file: File,
    /// Whether it is the file that was opened, rather than one that
    /// replaced it.
    opened: bool,
}

/// Where the file is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Presence {
    /// At [`Inner::path`].
    Here,
    /// Moved into a Trash folder.
    InTrash,
    /// Unlinked, with nothing at its path.
    Deleted,
    /// Its volume isn't mounted.
    Gone,
}

impl Original {
    /// Starts keeping track of the file at `path`, which was opened as
    /// `opened`. It looks at the file once straight away: if the path
    /// already leads to another file, or the file has changed since it was
    /// opened, it is `Changed` from the start.
    #[must_use]
    pub fn new(path: &Path, opened: FileIdentity) -> Self {
        let queue = Kqueue::new()
            .and_then(|queue| queue.add_user(STOP).map(|()| queue))
            .ok();
        let mut inner = Inner {
            path: path.to_owned(),
            pending: None,
            clock: Clock::default(),
            watched: None,
            presence: Presence::Here,
            diverged: false,
            written: false,
        };
        inner.first_look(&opened, queue.as_ref());
        let published = Mutex::new(inner.status());
        Self {
            shared: Arc::new(Shared {
                opened,
                queue,
                inner: Mutex::new(inner),
                published,
            }),
            thread: Mutex::new(None),
        }
    }

    /// The file as last seen. This makes no system calls, and never waits
    /// for a look at the file in progress (on the watching thread, or a
    /// [`check`](Self::check)): it copies the status published after the
    /// last one. Safe on the main thread.
    #[must_use]
    pub fn status(&self) -> OriginalStatus {
        self.shared.published()
    }

    /// Looks at the file now and returns what it found: the app calls it
    /// when a volume mounts (so a removable drive that is back is noticed)
    /// and when it becomes active. It makes a few system calls (`fstat`,
    /// `stat`, and `open` to watch a file that is back), so on a slow
    /// network volume, call it off the main thread.
    pub fn check(&self) -> OriginalStatus {
        let mut inner = self.shared.lock();
        inner.evaluate(&self.shared.opened, self.shared.queue.as_ref(), 0);
        self.shared.publish(&inner)
    }

    /// Starts a thread that waits for the file's events and calls
    /// `on_change` with the new status each time it changes. `on_change`
    /// runs on that thread; keep it short. A second call does nothing.
    ///
    /// # Errors
    ///
    /// If the kernel wouldn't make an event queue, or the thread couldn't
    /// be started.
    pub fn watch(&self, on_change: impl Fn(&OriginalStatus) + Send + 'static) -> io::Result<()> {
        let mut thread = self.thread.lock().unwrap_or_else(PoisonError::into_inner);
        if thread.is_some() {
            return Ok(());
        }
        if self.shared.queue.is_none() {
            return Err(io::Error::other("no event queue to watch the file with"));
        }
        let shared = Arc::clone(&self.shared);
        *thread = Some(
            std::thread::Builder::new()
                .name("leal-watch".to_owned())
                .spawn(move || shared.run(&on_change))?,
        );
        Ok(())
    }
}

impl Original {
    /// Holds the lock a look at the file holds, as a look blocked on a
    /// slow volume would (tests only).
    #[cfg(test)]
    pub(super) fn hold_as_a_look_would(&self) -> impl Sized + '_ {
        self.shared.lock()
    }

    /// Stops the clock pending moves are timed by (tests only): from now
    /// on it moves only by [`advance_clock`](Self::advance_clock).
    #[cfg(test)]
    pub(super) fn use_manual_clock(&self) {
        let mut inner = self.shared.lock();
        inner.clock.manual = Some(Instant::now());
    }

    /// Moves the stopped clock on by `by` (tests only).
    #[cfg(test)]
    pub(super) fn advance_clock(&self, by: Duration) {
        let mut inner = self.shared.lock();
        let now = inner.clock.manual.expect("the clock isn't stopped");
        inner.clock.manual = Some(now + by);
    }
}

impl Drop for Original {
    fn drop(&mut self) {
        let thread = self
            .thread
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let (Some(thread), Some(queue)) = (thread, &self.shared.queue)
            && queue.trigger(STOP).is_ok()
        {
            // It wakes at once: it only ever waits on the queue.
            let _ = thread.join();
        }
    }
}

impl std::fmt::Debug for Original {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Original")
            .field("status", &self.status())
            .finish_non_exhaustive()
    }
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Inner> {
        // Nothing that can panic runs while the lock is held, except a bug;
        // the state is still usable then.
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn published(&self) -> OriginalStatus {
        self.published
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Publishes `inner`'s status for `Original::status`, and returns it.
    /// Called with `inner` locked; the order is always `inner`, then
    /// `published`.
    fn publish(&self, inner: &Inner) -> OriginalStatus {
        let status = inner.status();
        *self
            .published
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = status.clone();
        status
    }

    /// The watching thread: wait, look, tell.
    fn run(&self, on_change: &dyn Fn(&OriginalStatus)) {
        let Some(queue) = &self.queue else { return };
        loop {
            let timeout = {
                let inner = self.lock();
                if inner.pending.is_some() {
                    Some(PENDING_POLL)
                } else {
                    (inner.presence == Presence::Deleted).then_some(DELETED_POLL)
                }
            };
            let Ok(events) = queue.wait(timeout) else {
                // The queue failed, which shouldn't happen: stop watching.
                // `check` still works.
                return;
            };
            if events.iter().any(|event| event.user && event.ident == STOP) {
                return;
            }
            let fflags = events
                .iter()
                .filter(|event| !event.user)
                .fold(0, |all, event| all | event.fflags);
            let changed = {
                let mut inner = self.lock();
                let before = inner.status();
                inner.evaluate(&self.opened, Some(queue), fflags);
                let after = self.publish(&inner);
                (after != before).then_some(after)
            };
            if let Some(status) = changed {
                on_change(&status);
            }
        }
    }
}

impl Inner {
    fn status(&self) -> OriginalStatus {
        let state = match self.presence {
            Presence::Here if self.diverged => OriginalState::Changed,
            Presence::Here => OriginalState::Unchanged,
            Presence::InTrash | Presence::Deleted => OriginalState::Deleted,
            Presence::Gone => OriginalState::Unavailable,
        };
        OriginalStatus {
            state,
            path: self.path.clone(),
            diverged: self.diverged,
            written: self.written,
        }
    }

    /// The look `Original::new` takes: open the file at the path for
    /// events, and compare it with what was opened, device included.
    fn first_look(&mut self, opened: &FileIdentity, queue: Option<&Kqueue>) {
        match watch_path(&self.path, queue) {
            Ok((file, now)) => {
                let same_file = now.dev() == opened.device && now.ino() == opened.inode;
                if !same_file {
                    // The path already leads to another file.
                    self.diverged = true;
                } else if !same_contents(opened, &now) {
                    self.diverged = true;
                    self.written = true;
                }
                self.presence = presence_at(&self.path);
                self.watched = Some(Watched {
                    file,
                    opened: same_file,
                });
            }
            Err(error) if is_missing(&error) => {
                self.presence = Presence::Deleted;
                self.diverged = true;
            }
            // It can't be watched (which shouldn't happen to a file Leal
            // could open). It is assumed to be there; `check` tries again.
            Err(_) => {}
        }
    }

    /// Looks at the file after events `fflags` (0 for an explicit check).
    fn evaluate(&mut self, opened: &FileIdentity, queue: Option<&Kqueue>, fflags: u32) {
        // A move that has stood for the window is followed now, whatever is
        // at the old path by then.
        if let Some(pending) = &self.pending
            && self.clock.now().saturating_duration_since(pending.since) >= MOVE_WINDOW
        {
            let to = pending.to.clone();
            self.pending = None;
            self.path = to;
        }
        match self.watched.take() {
            Some(watched) => self.follow(watched, opened, queue, fflags),
            None => self.look_again(opened, queue),
        }
    }

    /// The watched file after `fflags`.
    fn follow(
        &mut self,
        watched: Watched,
        opened: &FileIdentity,
        queue: Option<&Kqueue>,
        fflags: u32,
    ) {
        if watched.opened && fflags & WRITES != 0 {
            // Written, even if its size and modification time look the
            // same (the granularity limit in the module docs).
            self.diverged = true;
            self.written = true;
        }
        // A forced unmount revokes the descriptor, so `fstat` fails; an
        // ordinary one reports `NOTE_REVOKE`.
        let now = match watched.file.metadata() {
            Ok(now) if fflags & libc::NOTE_REVOKE == 0 => now,
            _ => {
                self.presence = Presence::Gone;
                // The volume may already be back (a check after it
                // mounted again, with no thread to see it go).
                drop(watched);
                self.look_again(opened, queue);
                return;
            }
        };
        if now.nlink() == 0 {
            // Unlinked: replaced by another file at its path (during a
            // pending move, still the old one: a backup-then-delete save),
            // or deleted.
            self.diverged = true;
            self.pending = None;
            drop(watched);
            if !self.replaced_at_path(queue) {
                self.presence = Presence::Deleted;
            }
            return;
        }
        // Renamed? The path known so far tells:
        // - it still leads to the watched file: not a move (the kernel's
        //   path is resolved, `/private/var/…` for `/var/…`, which isn't
        //   one);
        // - another regular file is there: a safe save swapped or renamed
        //   the new file in, so this is a replacement;
        // - nothing is there: a move, and the kernel knows the new path. It
        //   is pending until it has stood for the window.
        match fs::metadata(&self.path) {
            Ok(at_path) if at_path.dev() == now.dev() && at_path.ino() == now.ino() => {}
            Ok(at_path) if at_path.is_file() => {
                self.diverged = true;
                drop(watched);
                if !self.replaced_at_path(queue) {
                    // Gone again already: look once more next time.
                    self.presence = Presence::Here;
                }
                return;
            }
            _ => {
                if let Ok(to) = sys::path_of(&watched.file)
                    && to != self.path
                    && !in_temporary_items(&to)
                {
                    let since = self
                        .pending
                        .take()
                        .map_or_else(|| self.clock.now(), |pending| pending.since);
                    self.pending = Some(PendingMove { to, since });
                }
            }
        }
        if watched.opened && !same_contents(opened, &now) {
            self.diverged = true;
            self.written = true;
        }
        self.presence = presence_at(&self.path);
        self.watched = Some(watched);
    }

    /// After the watched file was unlinked or renamed away: if a regular
    /// file is at the path, it replaced the one opened (a change) and is
    /// watched from now on. Returns whether one was there.
    fn replaced_at_path(&mut self, queue: Option<&Kqueue>) -> bool {
        match watch_path(&self.path, queue) {
            Ok((file, _)) => {
                self.diverged = true;
                self.pending = None;
                self.presence = presence_at(&self.path);
                self.watched = Some(Watched {
                    file,
                    opened: false,
                });
                true
            }
            Err(error) if is_missing(&error) => false,
            // Something is there that can't be watched: say it's there,
            // and look again next time.
            Err(_) => {
                self.diverged = true;
                self.presence = Presence::Here;
                true
            }
        }
    }

    /// Nothing is watched (the file was deleted, its volume went away, or
    /// it couldn't be watched): look for a file at the path again. A file
    /// that is back is compared with what was opened by inode, size and
    /// modification time, not by device, because a volume gets a new
    /// device number each time it mounts.
    fn look_again(&mut self, opened: &FileIdentity, queue: Option<&Kqueue>) {
        let Ok((file, now)) = watch_path(&self.path, queue) else {
            // Still not there (a missing path while the volume is away
            // can't be told from a deleted file, so it stays as it was).
            return;
        };
        let same = now.ino() == opened.inode && same_contents(opened, &now);
        if !same {
            self.diverged = true;
            if now.ino() == opened.inode {
                self.written = true;
            }
        }
        self.presence = presence_at(&self.path);
        self.watched = Some(Watched { file, opened: same });
    }
}

/// Opens the file at `path` for events, adds it to `queue`, and then reads
/// its metadata (after adding it, so no change in between is missed). A
/// path that leads to something other than a regular file counts as
/// missing.
fn watch_path(path: &Path, queue: Option<&Kqueue>) -> io::Result<(File, fs::Metadata)> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_EVTONLY | libc::O_NONBLOCK)
        .open(path)?;
    if let Some(queue) = queue {
        queue.watch(&file, EVENTS)?;
    }
    let now = file.metadata()?;
    if !now.is_file() {
        return Err(io::Error::from(io::ErrorKind::NotFound));
    }
    Ok((file, now))
}

/// Whether an error opening the path means nothing is there.
fn is_missing(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
    )
}

/// Whether the file `now` describes has the size and modification time it
/// had when opened (`then`).
fn same_contents(then: &FileIdentity, now: &fs::Metadata) -> bool {
    now.len() == then.len && now.modified().ok() == then.modified
}

/// Whether `path` is inside a temporary-items folder (`TemporaryItems` or a
/// volume's `.TemporaryItems`), where Foundation stages safe saves
/// (`FileManager.url(for: .itemReplacementDirectory, …)`). A file moved
/// there is passing through, not moved by the user.
fn in_temporary_items(path: &Path) -> bool {
    path.components().any(|component| {
        matches!(component, Component::Normal(name) if name == "TemporaryItems" || name == ".TemporaryItems")
    })
}

/// `Here`, or `InTrash` if `path` is inside a Trash folder: the user's
/// (`~/.Trash`) or a volume's (`.Trashes`).
fn presence_at(path: &Path) -> Presence {
    let in_trash = path.components().any(|component| {
        matches!(component, Component::Normal(name) if name == ".Trash" || name == ".Trashes")
    });
    if in_trash {
        Presence::InTrash
    } else {
        Presence::Here
    }
}
