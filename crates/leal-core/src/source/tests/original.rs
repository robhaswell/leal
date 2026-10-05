//! Watching the user's file (task 1.9): [`Original`] on ordinary files in
//! a temporary directory. The removable-drive cases (a volume that goes
//! away and comes back) are in `removable.rs`, with disk images.

use super::*;
use std::fs::OpenOptions as FsOpenOptions;
use std::io::Write as _;
use std::os::unix::fs::FileExt as _;
use std::sync::{Mutex, mpsc};
use std::time::{Duration, Instant, SystemTime};

/// How long a test waits for the watcher to notice something.
const NOTICE: Duration = Duration::from_secs(5);

/// The bound on a call that must not block (on a busy thread, a held look):
/// long enough that a busy machine never reaches it (task 2.G-b), far
/// shorter than the waits the tests hold the other side for.
const NOT_BLOCKED: Duration = Duration::from_secs(5);

/// The file at `path` as `open` would record it.
fn identity(path: &Path) -> FileIdentity {
    open_regular(path).unwrap().1
}

/// An [`Original`] for `path` as it is now, watched, and a channel that
/// receives every status the watcher reports.
fn watched(path: &Path) -> (Original, mpsc::Receiver<OriginalStatus>) {
    let original = Original::new(path, identity(path));
    let (send, receive) = mpsc::channel();
    original
        .watch(move |status| {
            let _ = send.send(status.clone());
        })
        .unwrap();
    (original, receive)
}

/// Waits until the watcher reports a status that `done` accepts, and
/// returns it.
fn wait_for(
    reports: &mpsc::Receiver<OriginalStatus>,
    what: &str,
    done: impl Fn(&OriginalStatus) -> bool,
) -> OriginalStatus {
    let deadline = Instant::now() + NOTICE;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match reports.recv_timeout(left) {
            Ok(status) if done(&status) => return status,
            Ok(_) => {}
            Err(_) => panic!("the watcher didn't report {what} within {NOTICE:?}"),
        }
    }
}

/// Asserts that the watcher reports nothing for a while.
fn assert_quiet(reports: &mpsc::Receiver<OriginalStatus>) {
    if let Ok(status) = reports.recv_timeout(Duration::from_millis(300)) {
        panic!("the watcher reported {status:?}, but nothing changed");
    }
}

fn state(state: OriginalState) -> impl Fn(&OriginalStatus) -> bool {
    move |status| status.state == state
}

/// Writes `bytes` over the start of the file, in place (same inode).
fn write_in_place(path: &Path, bytes: &[u8]) {
    let file = FsOpenOptions::new().write(true).open(path).unwrap();
    file.write_all_at(bytes, 0).unwrap();
}

#[test]
fn an_unchanged_file_stays_unchanged() {
    let dir = TempDir::new("original");
    let path = dir.file("a.csv", b"a,b\n1,2\n");
    let (original, reports) = watched(&path);
    let status = original.status();
    assert_eq!(status.state, OriginalState::Unchanged);
    assert_eq!(status.path, path);
    assert!(!status.diverged);
    assert_quiet(&reports);
    assert_eq!(original.check().state, OriginalState::Unchanged);
}

#[test]
fn a_write_in_place_is_a_change() {
    let dir = TempDir::new("original");
    let path = dir.file("a.csv", b"a,b\n1,2\n");
    let (original, reports) = watched(&path);
    write_in_place(&path, b"x,y");
    let status = wait_for(&reports, "the write", state(OriginalState::Changed));
    assert!(status.diverged);
    assert!(status.written, "the file that was opened was written");
    assert_eq!(status.path, path);
    assert_eq!(original.status(), status);
}

#[test]
fn truncating_or_appending_is_a_change() {
    let dir = TempDir::new("original");
    let truncated = dir.file("t.csv", b"a,b\n1,2\n3,4\n");
    let appended = dir.file("e.csv", b"a,b\n1,2\n");
    let (_t, truncations) = watched(&truncated);
    let (_e, appends) = watched(&appended);

    FsOpenOptions::new()
        .write(true)
        .open(&truncated)
        .unwrap()
        .set_len(4)
        .unwrap();
    wait_for(
        &truncations,
        "the truncation",
        state(OriginalState::Changed),
    );

    let mut file = FsOpenOptions::new().append(true).open(&appended).unwrap();
    file.write_all(b"5,6\n").unwrap();
    let status = wait_for(&appends, "the append", state(OriginalState::Changed));
    assert!(status.written);
}

#[test]
fn deleting_the_file_is_noticed_and_a_new_file_there_is_a_change() {
    let dir = TempDir::new("original");
    let path = dir.file("a.csv", b"a,b\n1,2\n");
    let (original, reports) = watched(&path);
    fs::remove_file(&path).unwrap();
    let status = wait_for(&reports, "the deletion", state(OriginalState::Deleted));
    assert!(status.diverged);
    assert_eq!(status.path, path);

    // Some tools delete a file and write a new one: the path is looked at
    // while the file is deleted.
    fs::write(&path, b"a,b\n9,9\n").unwrap();
    let status = wait_for(&reports, "the new file", state(OriginalState::Changed));
    assert!(status.diverged);
    assert!(!status.written, "the opened file itself wasn't written");
    assert_eq!(original.check().state, OriginalState::Changed);
}

#[test]
fn a_renamed_file_is_followed() {
    let dir = TempDir::new("original");
    let path = dir.file("a.csv", b"a,b\n1,2\n");
    let (original, reports) = watched(&path);
    let moved = dir.folder("elsewhere").join("renamed.csv");
    fs::rename(&path, &moved).unwrap();
    let status = wait_for(&reports, "the rename", |status| status.path == moved);
    assert_eq!(
        status.state,
        OriginalState::Unchanged,
        "only its name changed"
    );
    assert!(!status.diverged);

    // It is still watched under its new name.
    write_in_place(&moved, b"x");
    let status = wait_for(
        &reports,
        "a write after the rename",
        state(OriginalState::Changed),
    );
    assert_eq!(status.path, moved);
    assert_eq!(original.check().path, moved);
}

#[test]
fn moving_the_file_to_the_trash_counts_as_deleting_it() {
    let dir = TempDir::new("original");
    let path = dir.file("a.csv", b"a,b\n1,2\n");
    let (_original, reports) = watched(&path);
    let trash = dir.folder(".Trash").join("a.csv");
    fs::rename(&path, &trash).unwrap();
    let status = wait_for(
        &reports,
        "the move to the Trash",
        state(OriginalState::Deleted),
    );
    assert_eq!(status.path, trash);
    assert!(!status.diverged, "its contents are as they were");

    // Put back.
    fs::rename(&trash, &path).unwrap();
    let status = wait_for(&reports, "the move back", state(OriginalState::Unchanged));
    assert_eq!(status.path, path);
}

#[test]
fn changing_only_attributes_is_not_a_change() {
    let dir = TempDir::new("original");
    let path = dir.file("a.csv", b"a,b\n1,2\n");
    let (original, reports) = watched(&path);
    set_attribute(&path, "com.example.tag", "01");
    set_attribute(
        &path,
        TEXT_ENCODING_ATTRIBUTE,
        "7574662d383b313334323137393834",
    );
    fs::set_permissions(&path, Permissions::from_mode(0o444)).unwrap();
    assert_quiet(&reports);
    let status = original.check();
    assert_eq!(status.state, OriginalState::Unchanged);
    assert!(!status.diverged);
}

#[test]
fn touching_the_modification_time_is_a_change() {
    // Leal can't tell the contents are the same, so it says so.
    let dir = TempDir::new("original");
    let path = dir.file("a.csv", b"a,b\n1,2\n");
    let (_original, reports) = watched(&path);
    let file = File::options().write(true).open(&path).unwrap();
    file.set_modified(SystemTime::now() + Duration::from_secs(60))
        .unwrap();
    let status = wait_for(&reports, "the new time", state(OriginalState::Changed));
    assert!(status.written);
}

#[test]
fn a_file_replaced_by_rename_is_a_change_and_the_new_one_is_watched() {
    // How most apps save: write a new file, then rename it over the old.
    let dir = TempDir::new("original");
    let path = dir.file("a.csv", b"a,b\n1,2\n");
    let (original, reports) = watched(&path);
    let new = dir.file("a.csv.tmp", b"a,b\n3,4\n");
    fs::rename(&new, &path).unwrap();
    let status = wait_for(&reports, "the replacement", state(OriginalState::Changed));
    assert_eq!(status.path, path);
    assert!(!status.written, "the opened file itself wasn't written");

    // The new file is watched: deleting it is noticed.
    fs::remove_file(&path).unwrap();
    wait_for(
        &reports,
        "the new file's deletion",
        state(OriginalState::Deleted),
    );
    assert_eq!(original.status().state, OriginalState::Deleted);
}

#[test]
fn a_same_size_write_that_keeps_the_time_is_caught_only_by_its_event() {
    // The modification-time granularity limit (module docs of
    // `source::original`): a same-size write within the volume's
    // granularity leaves the size and time as they were. Setting the time
    // back reproduces that here, on APFS (1 ns).
    let dir = TempDir::new("original");
    let path = dir.file("a.csv", b"a,b\n1,2\n");
    let opened = identity(&path);
    // Both are registered with their queues now; the watching thread starts
    // only after the change, so all it has to go on is the queued events
    // (it can't catch the time between the write and its reset).
    let watching = Original::new(&path, opened);
    let looking = Original::new(&path, opened);

    write_in_place(&path, b"x,y\n9,9\n");
    let file = File::options().write(true).open(&path).unwrap();
    file.set_modified(opened.modified.unwrap()).unwrap();
    assert_eq!(identity(&path).len, opened.len);
    assert_eq!(identity(&path).modified, opened.modified);

    // The watcher saw the write event.
    let (send, reports) = mpsc::channel();
    watching
        .watch(move |status| {
            let _ = send.send(status.clone());
        })
        .unwrap();
    wait_for(&reports, "the write", state(OriginalState::Changed));
    assert_eq!(watching.status().state, OriginalState::Changed);
    // A look at the file alone (all there is when its volume was away)
    // can't tell.
    assert_eq!(looking.check().state, OriginalState::Unchanged);
}

#[test]
fn a_change_before_watching_starts_is_found_by_the_first_look() {
    let dir = TempDir::new("original");
    let path = dir.file("a.csv", b"a,b\n1,2\n");
    let opened = identity(&path);
    fs::write(&path, b"a,b\n1,2\n3,4\n").unwrap();
    let status = Original::new(&path, opened).status();
    assert_eq!(status.state, OriginalState::Changed);
    assert!(status.written);

    // Replaced before watching started.
    let other = dir.file("b.csv", b"a,b\n1,2\n");
    let opened = identity(&other);
    let new = dir.file("b.tmp", b"a,b\n1,2\n");
    fs::rename(&new, &other).unwrap();
    let status = Original::new(&other, opened).status();
    assert_eq!(status.state, OriginalState::Changed);
    assert!(!status.written);

    // Deleted before watching started.
    let gone = dir.file("c.csv", b"x\n");
    let opened = identity(&gone);
    fs::remove_file(&gone).unwrap();
    assert_eq!(
        Original::new(&gone, opened).status().state,
        OriginalState::Deleted
    );
}

#[test]
fn dropping_a_watched_original_stops_its_thread() {
    let dir = TempDir::new("original");
    let path = dir.file("a.csv", b"a,b\n");
    let (original, reports) = watched(&path);
    let start = Instant::now();
    drop(original);
    // Only against a hang: a busy machine can take a while (task 2.G-b).
    assert!(start.elapsed() < NOT_BLOCKED);
    // The thread is gone, so the channel's sender is too.
    assert!(matches!(
        reports.recv_timeout(Duration::from_secs(1)),
        Err(mpsc::RecvTimeoutError::Disconnected)
    ));
}

#[test]
fn watching_twice_is_harmless() {
    let dir = TempDir::new("original");
    let path = dir.file("a.csv", b"a,b\n");
    let (original, reports) = watched(&path);
    original.watch(|_| {}).unwrap();
    write_in_place(&path, b"z");
    wait_for(&reports, "the write", state(OriginalState::Changed));
}

/// The watching thread may be busy after an event: looking at the file
/// (which can block on a hung network volume) or in `on_change`. Dropping
/// the `Original`, which the app does on the main thread, doesn't wait for
/// it (p1-review conc-4); the thread stops by itself once it is done.
#[test]
fn dropping_doesnt_wait_for_a_busy_watching_thread() {
    let dir = TempDir::new("original");
    let path = dir.file("a.csv", b"a,b\n");
    let original = Original::new(&path, identity(&path));
    let (busy_tx, busy) = mpsc::channel();
    let (release, released) = mpsc::channel::<()>();
    let released = Mutex::new(released);
    original
        .watch(move |_| {
            let _ = busy_tx.send(());
            // Busy, as a look blocked on a hung volume would be.
            let _ = released
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(10));
        })
        .unwrap();
    write_in_place(&path, b"z");
    busy.recv_timeout(NOTICE).unwrap();
    let start = Instant::now();
    drop(original);
    let took = start.elapsed();
    // The busy thread is held for 10 s, so a drop that waited for it
    // would take that long; anything less is a busy machine (task 2.G-b).
    assert!(took < NOT_BLOCKED, "dropping waited {took:?}");
    release.send(()).unwrap();
}

/// When the kernel won't make an event queue (as when the process is out
/// of descriptors, p1-review conc-6), watching fails and says why, rather
/// than leaving the document unwatched without a word.
#[test]
fn watching_without_an_event_queue_says_why() {
    let dir = TempDir::new("original");
    let path = dir.file("a.csv", b"a,b\n");
    let original = Original::without_queue(
        &path,
        identity(&path),
        io::Error::from_raw_os_error(libc::EMFILE),
    );
    let error = original.watch(|_| {}).unwrap_err();
    let message = error.to_string();
    assert!(
        message.contains("event queue") && message.contains("Too many open files"),
        "{message}"
    );
    // A look still works.
    write_in_place(&path, b"z");
    assert_eq!(original.check().state, OriginalState::Changed);
}

// ---------------------------------------------------------------------------
// Safe saves (review of task 1.9): another file takes the path while the
// opened one is renamed away. Each is a replacement, never a move or a
// deletion, and no status ever names a staging path.

/// Collects every status until one has state `last`, and checks that none
/// of them was `Deleted` or named a path other than `path`.
fn replaced_without_detour(
    reports: &mpsc::Receiver<OriginalStatus>,
    path: &Path,
) -> Vec<OriginalStatus> {
    let mut seen = Vec::new();
    let deadline = Instant::now() + NOTICE;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let status = reports
            .recv_timeout(left)
            .unwrap_or_else(|_| panic!("no replacement reported; saw {seen:?}"));
        assert_eq!(
            status.path, path,
            "the document stays at its path: {status:?}"
        );
        assert_ne!(
            status.state,
            OriginalState::Deleted,
            "never deleted: {status:?}"
        );
        let done = status.state == OriginalState::Changed;
        seen.push(status);
        if done {
            return seen;
        }
    }
}

#[test]
fn a_swap_save_is_a_replacement() {
    // `FileManager.replaceItemAt`: the new file is written in a
    // replacement folder, swapped with the original (`RENAME_SWAP`), and
    // the old one, now in the folder, is deleted.
    let dir = TempDir::new("original");
    let path = dir.file("x.csv", b"a,b\n1,2\n");
    let (original, reports) = watched(&path);
    let staging = dir.folder("TemporaryItems/NSIRD_save_1");
    let new = staging.join("x.csv");
    fs::write(&new, b"a,b\n3,4\n5,6\n").unwrap();
    sys::swap(&new, &path).unwrap();
    fs::remove_file(&new).unwrap();
    fs::remove_dir(&staging).unwrap();
    replaced_without_detour(&reports, &path);
    // Settled: still changed, at the path, and the new file is watched.
    std::thread::sleep(Duration::from_millis(100));
    let status = original.check();
    assert_eq!(status.state, OriginalState::Changed);
    assert_eq!(status.path, path);
    fs::remove_file(&path).unwrap();
    wait_for(
        &reports,
        "the new file's deletion",
        state(OriginalState::Deleted),
    );
    assert_eq!(original.status().path, path);
}

#[test]
fn a_save_through_a_temporary_folder_is_a_replacement() {
    // A safe save without the swap: the original is moved into a
    // temporary-items folder, the new file is moved into place, and the
    // old one is deleted.
    let dir = TempDir::new("original");
    let path = dir.file("x.csv", b"a,b\n1,2\n");
    let (original, reports) = watched(&path);
    let staging = dir.folder("TemporaryItems/NSIRD_save_2");
    let backup = staging.join("x.csv");
    let new = dir.file("x.csv.new", b"a,b\n3,4\n");
    fs::rename(&path, &backup).unwrap();
    // Let the watcher see the file sitting in the staging folder.
    std::thread::sleep(Duration::from_millis(100));
    fs::rename(&new, &path).unwrap();
    fs::remove_file(&backup).unwrap();
    replaced_without_detour(&reports, &path);
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(original.check().path, path);
}

#[test]
fn a_backup_rename_then_write_is_a_replacement() {
    // Emacs: rename the file to `x.csv~`, write a new `x.csv`, delete the
    // backup.
    let dir = TempDir::new("original");
    let path = dir.file("x.csv", b"a,b\n1,2\n");
    let (original, reports) = watched(&path);
    let backup = dir.0.join("x.csv~");
    fs::rename(&path, &backup).unwrap();
    // The watcher may see the rename as a move before the new file comes.
    std::thread::sleep(Duration::from_millis(100));
    fs::write(&path, b"a,b\n3,4\n").unwrap();
    fs::remove_file(&backup).unwrap();
    let status = wait_for(&reports, "the replacement", |s| {
        s.state == OriginalState::Changed && s.path == path
    });
    assert!(status.diverged);
    std::thread::sleep(Duration::from_millis(100));
    let settled = original.check();
    assert_eq!(settled.state, OriginalState::Changed);
    assert_eq!(settled.path, path);
}

#[test]
fn a_backup_deleted_before_the_new_file_comes_is_a_replacement() {
    // The same, with the backup deleted first: for a moment the file is
    // deleted, then the new one appears at the old path.
    let dir = TempDir::new("original");
    let path = dir.file("x.csv", b"a,b\n1,2\n");
    let (_original, reports) = watched(&path);
    let backup = dir.0.join("x.csv~");
    fs::rename(&path, &backup).unwrap();
    std::thread::sleep(Duration::from_millis(100));
    fs::remove_file(&backup).unwrap();
    std::thread::sleep(Duration::from_millis(100));
    fs::write(&path, b"a,b\n3,4\n").unwrap();
    wait_for(&reports, "the new file", |s| {
        s.state == OriginalState::Changed && s.path == path
    });
}

#[test]
fn status_never_waits_for_a_look_in_progress() {
    // The main thread reads `status()` (through `Document::can_save`)
    // while a look at the file may be blocked on a slow volume.
    let dir = TempDir::new("original");
    let path = dir.file("x.csv", b"a,b\n");
    let original = Original::new(&path, identity(&path));
    let held = original.hold_as_a_look_would();
    let start = Instant::now();
    assert_eq!(original.status().state, OriginalState::Unchanged);
    // The look is held until the end of the test, so a `status()` that
    // waited for it would never return: the bound is only against a hang.
    assert!(start.elapsed() < NOT_BLOCKED);
    drop(held);
}

#[test]
fn a_save_that_keeps_its_backup_is_a_replacement() {
    // Emacs's default: rename the file to `x.csv~`, keep it, and write a
    // new `x.csv`. No event reaches the renamed file after the rename, so
    // the old path is looked at while the move is pending.
    let dir = TempDir::new("original");
    let path = dir.file("x.csv", b"a,b\n1,2\n");
    let (original, reports) = watched(&path);
    let backup = dir.0.join("x.csv~");
    fs::rename(&path, &backup).unwrap();
    // Let the watcher see the rename before the new file comes.
    std::thread::sleep(Duration::from_millis(150));
    assert_eq!(
        original.status().path,
        path,
        "a pending move isn't reported"
    );
    fs::write(&path, b"a,b\n3,4\n").unwrap();
    replaced_without_detour(&reports, &path);
    // Settled, well past the window: still the file at its path.
    let status = original.check();
    assert_eq!(status.state, OriginalState::Changed);
    assert_eq!(status.path, path);
    assert!(backup.exists());
}

#[test]
fn a_file_at_the_old_path_within_the_window_is_a_replacement() {
    let dir = TempDir::new("original");
    let path = dir.file("x.csv", b"a,b\n1,2\n");
    let original = Original::new(&path, identity(&path));
    original.use_manual_clock();
    fs::rename(&path, dir.0.join("x.csv~")).unwrap();
    let pending = original.check();
    assert_eq!(pending.state, OriginalState::Unchanged);
    assert_eq!(pending.path, path, "the move is pending");

    original.advance_clock(MOVE_WINDOW - Duration::from_millis(1));
    fs::write(&path, b"a,b\n3,4\n").unwrap();
    let status = original.check();
    assert_eq!(status.state, OriginalState::Changed);
    assert_eq!(status.path, path);
    // And it stays so: the window doesn't move the document afterwards.
    original.advance_clock(MOVE_WINDOW * 2);
    assert_eq!(original.check().path, path);
}

#[test]
fn a_move_that_stands_for_the_window_is_followed_and_the_old_path_forgotten() {
    let dir = TempDir::new("original");
    let path = dir.file("x.csv", b"a,b\n1,2\n");
    let original = Original::new(&path, identity(&path));
    original.use_manual_clock();
    let moved = dir.folder("elsewhere").join("x.csv");
    fs::rename(&path, &moved).unwrap();
    assert_eq!(original.check().path, path, "pending");
    original.advance_clock(MOVE_WINDOW - Duration::from_millis(1));
    assert_eq!(original.check().path, path, "still pending");
    original.advance_clock(Duration::from_millis(1));
    let status = original.check();
    assert_eq!(status.path, moved, "followed");
    assert_eq!(status.state, OriginalState::Unchanged);

    // An unrelated file at the old path, then the moved file deleted: the
    // old path was forgotten, so this is a deletion, not a replacement.
    fs::write(&path, b"unrelated\n").unwrap();
    assert_eq!(original.check().state, OriginalState::Unchanged);
    fs::remove_file(&moved).unwrap();
    let status = original.check();
    assert_eq!(status.state, OriginalState::Deleted);
    assert_eq!(status.path, moved);
}
