//! The file watcher, against a real directory on a real filesystem.
//!
//! Every piece of this had unit tests and they all passed. What none of them
//! exercised was the loop that actually runs — and the loop was inert. The
//! debouncer was constructed *inside* the function that drains it, so its
//! entries were always microseconds old, always younger than the 200 ms debounce
//! delay, and always thrown away with the debouncer at the end of the call.
//! `spawn_file_watcher` emitted nothing at all, for the life of the program.
//!
//! `test_adaptive_debouncer` passed throughout, because it sleeps between adding
//! an event and draining — which is the one sequence production never performs.
//! That is the seventh time in this project a test has encoded the bug rather
//! than catching it, and the answer is the same as it was for the tool layer:
//! drive the real entry point against a real filesystem.
//!
//! These are timing tests, which is unavoidable — the thing under test is a
//! debounce. The waits are generous relative to the 200 ms delay and the 50 ms
//! poll, and the assertions are about *whether* an event arrives, never about
//! how quickly.

use std::time::{Duration, Instant};

use smithy_editor::{spawn_file_watcher, FileWatcherEvent, IdeFileChange};

/// Collect changes until `want` is satisfied or the deadline passes.
///
/// Returns everything seen, so a failure can print what did arrive rather than
/// only what did not.
fn collect_until(
    rx: &crossbeam_channel::Receiver<FileWatcherEvent>,
    timeout: Duration,
    want: impl Fn(&[IdeFileChange]) -> bool,
) -> Vec<IdeFileChange> {
    let deadline = Instant::now() + timeout;
    let mut seen = Vec::new();
    while Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(remaining.min(Duration::from_millis(200))) {
            Ok(FileWatcherEvent::Changes(changes)) => {
                seen.extend(changes);
                if want(&seen) {
                    return seen;
                }
            }
            Ok(FileWatcherEvent::Error(e)) => panic!("watcher reported an error: {e}"),
            Err(_) => continue,
        }
    }
    seen
}

fn names(changes: &[IdeFileChange]) -> Vec<String> {
    changes
        .iter()
        .map(|c| match c {
            IdeFileChange::FileModified { path, external } => {
                format!("modified({}, external={external})", file_name(path))
            }
            IdeFileChange::FileCreated { path } => format!("created({})", file_name(path)),
            IdeFileChange::FileDeleted { path } => format!("deleted({})", file_name(path)),
            IdeFileChange::FileRenamed { from, to } => {
                format!("renamed({} -> {})", file_name(from), file_name(to))
            }
            IdeFileChange::DirectoryChanged { path } => {
                format!("dir_changed({})", file_name(path))
            }
            IdeFileChange::GitStatusChanged => "git_status".to_string(),
        })
        .collect()
}

fn file_name(path: &std::path::Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| path.display().to_string())
}

/// Spawn used to walk every directory on the caller. During launch that caller
/// is the UI thread, so a large Project made the window look crashed. Returning
/// in milliseconds against a tree that would take far longer to watch
/// directory-by-directory is the regression.
#[test]
fn spawn_returns_before_the_tree_is_walked() {
    let dir = tempfile::tempdir().expect("tempdir");
    for i in 0..80 {
        let nested = dir.path().join(format!("d{i}")).join("inner");
        std::fs::create_dir_all(&nested).expect("dirs");
        std::fs::write(nested.join("f.txt"), "x").expect("file");
    }
    let start = Instant::now();
    let (_handle, _rx) = spawn_file_watcher(dir.path().to_path_buf()).expect("spawn");
    let elapsed = start.elapsed();
    assert!(
        elapsed < Duration::from_millis(100),
        "spawn blocked on the watch/walk ({elapsed:?}) — that is the no-window hang"
    );
}

/// Whether any change concerns `name`.
fn touches(changes: &[IdeFileChange], name: &str) -> bool {
    changes.iter().any(|c| match c {
        IdeFileChange::FileModified { path, .. }
        | IdeFileChange::FileCreated { path }
        | IdeFileChange::FileDeleted { path } => file_name(path) == name,
        _ => false,
    })
}

/// **The test that would have caught it.** A file edited outside the editor has
/// to reach the editor, and before this the answer was silence.
#[test]
fn an_edit_made_outside_the_editor_is_reported() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("seed.txt"), "one\n").expect("seed");

    let (_handle, rx) = spawn_file_watcher(dir.path().to_path_buf()).expect("watcher starts");
    // The initial walk has to finish before a change can be noticed.
    std::thread::sleep(Duration::from_millis(300));

    std::fs::write(dir.path().join("seed.txt"), "two\n").expect("edit");

    let seen = collect_until(&rx, Duration::from_secs(5), |c| touches(c, "seed.txt"));
    assert!(
        touches(&seen, "seed.txt"),
        "an external edit produced no event in five seconds — the watcher is inert. Saw: {:?}",
        names(&seen)
    );
}

/// A file that was already there and is then edited is a **modification**, not a
/// creation. That distinction only holds because the initial walk seeds the
/// known-paths set; without it the first event about any pre-existing file
/// claims the file just appeared.
#[test]
fn editing_a_file_that_already_existed_reads_as_a_modification() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("seed.txt"), "one\n").expect("seed");

    let (_handle, rx) = spawn_file_watcher(dir.path().to_path_buf()).expect("watcher starts");
    std::thread::sleep(Duration::from_millis(300));

    std::fs::write(dir.path().join("seed.txt"), "two\n").expect("edit");

    let seen = collect_until(&rx, Duration::from_secs(5), |c| touches(c, "seed.txt"));
    assert!(
        seen.iter().any(|c| matches!(
            c,
            IdeFileChange::FileModified { path, .. } if file_name(path) == "seed.txt"
        )),
        "expected a modification for a file that already existed. Saw: {:?}",
        names(&seen)
    );
    assert!(
        !seen.iter().any(|c| matches!(
            c,
            IdeFileChange::FileCreated { path } if file_name(path) == "seed.txt"
        )),
        "seed.txt existed before the watch began and must not be reported as created. Saw: {:?}",
        names(&seen)
    );
}

/// A new file is a creation, **and** the directory is reported as changed —
/// which is the event the explorer redraws from. Without the second the tree
/// silently goes stale.
#[test]
fn a_new_file_is_reported_as_a_creation_and_changes_its_directory() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (_handle, rx) = spawn_file_watcher(dir.path().to_path_buf()).expect("watcher starts");
    std::thread::sleep(Duration::from_millis(300));

    std::fs::write(dir.path().join("fresh.txt"), "new\n").expect("create");

    let seen = collect_until(&rx, Duration::from_secs(5), |c| touches(c, "fresh.txt"));
    assert!(
        seen.iter().any(|c| matches!(
            c,
            IdeFileChange::FileCreated { path } if file_name(path) == "fresh.txt"
        )),
        "expected a creation. Saw: {:?}",
        names(&seen)
    );
    assert!(
        seen.iter()
            .any(|c| matches!(c, IdeFileChange::DirectoryChanged { .. })),
        "the explorer redraws from this, so a new file has to change its directory. Saw: {:?}",
        names(&seen)
    );
}

/// **The case macOS gets wrong if you trust the backend.** FSEvents replays a
/// coalesced flag set, so a deleted file's last reported kind is `Modify` — and
/// a debouncer keyed by path keeps the last kind. Classifying by whether the
/// path still exists is what makes this come out as a deletion.
#[test]
fn a_deleted_file_is_reported_as_deleted_and_not_as_a_modification() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("doomed.txt"), "here\n").expect("seed");

    let (_handle, rx) = spawn_file_watcher(dir.path().to_path_buf()).expect("watcher starts");
    std::thread::sleep(Duration::from_millis(300));

    std::fs::remove_file(dir.path().join("doomed.txt")).expect("delete");

    let seen = collect_until(&rx, Duration::from_secs(5), |c| touches(c, "doomed.txt"));
    assert!(
        seen.iter().any(|c| matches!(
            c,
            IdeFileChange::FileDeleted { path } if file_name(path) == "doomed.txt"
        )),
        "a removed file must be reported as deleted, whatever kind the backend last said. \
         Saw: {:?}",
        names(&seen)
    );
}

/// An atomic save — write a temp file, rename it over the original — is how most
/// editors save, and it must read as a modification rather than as a delete
/// followed by a create. This is what the removed `AtomicSaveDetector` existed
/// for; deciding by existence covers it, because both halves land inside one
/// debounce window and the file is there when the window drains.
#[test]
fn an_atomic_save_reads_as_a_modification_rather_than_a_delete() {
    let dir = tempfile::tempdir().expect("tempdir");
    let target = dir.path().join("saved.txt");
    std::fs::write(&target, "before\n").expect("seed");

    let (_handle, rx) = spawn_file_watcher(dir.path().to_path_buf()).expect("watcher starts");
    std::thread::sleep(Duration::from_millis(300));

    // Exactly what `Buffer::save_to_path` does.
    let temp = dir.path().join(".saved.txt.tmp");
    std::fs::write(&temp, "after\n").expect("temp write");
    std::fs::rename(&temp, &target).expect("rename over");

    let seen = collect_until(&rx, Duration::from_secs(5), |c| touches(c, "saved.txt"));
    assert!(
        seen.iter().any(|c| matches!(
            c,
            IdeFileChange::FileModified { path, .. } if file_name(path) == "saved.txt"
        )),
        "an atomic save is a modification. Saw: {:?}",
        names(&seen)
    );
    assert!(
        !seen.iter().any(|c| matches!(
            c,
            IdeFileChange::FileDeleted { path } if file_name(path) == "saved.txt"
        )),
        "the file is still there — reporting it deleted would close the tab. Saw: {:?}",
        names(&seen)
    );
}

/// Gitignored files are not the editor's business, and `target/` alone would
/// otherwise produce thousands of events per build.
#[test]
fn a_gitignored_file_produces_no_events() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join(".gitignore"), "ignored.txt\n").expect("gitignore");
    std::fs::write(dir.path().join("watched.txt"), "one\n").expect("seed");

    let (_handle, rx) = spawn_file_watcher(dir.path().to_path_buf()).expect("watcher starts");
    std::thread::sleep(Duration::from_millis(300));

    std::fs::write(dir.path().join("ignored.txt"), "noise\n").expect("ignored write");
    std::fs::write(dir.path().join("watched.txt"), "two\n").expect("watched write");

    // Wait for the *watched* file, so the ignored one has had at least as long.
    let seen = collect_until(&rx, Duration::from_secs(5), |c| touches(c, "watched.txt"));
    assert!(
        touches(&seen, "watched.txt"),
        "the negative control has to fire, or this test proves nothing. Saw: {:?}",
        names(&seen)
    );
    assert!(
        !touches(&seen, "ignored.txt"),
        "a gitignored file must not reach the editor. Saw: {:?}",
        names(&seen)
    );
}
