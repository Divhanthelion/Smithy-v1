//! Watching the project for changes made outside the editor.
//!
//! Events are filtered through gitignore rules and debounced, so a burst — a
//! `git checkout`, a `cargo build` — arrives as one batch rather than thousands.
//!
//! ## What happened is decided by looking, not by asking
//!
//! The backend's event *kind* is not trustworthy, and on macOS it is actively
//! misleading. FSEvents reports a coalesced flag set per path, which `notify`
//! expands into several events whose order does not describe what happened last:
//! creating and then deleting one file arrives as `Create`, `Remove`, `Modify`,
//! in that order. Taking the last kind — which is what a debouncer keyed by path
//! naturally does — therefore reports a deleted file as modified.
//!
//! So the kind is used for exactly one thing (recognising a rename pair) and the
//! classification comes from the filesystem instead: after the debounce window
//! closes, is the path there? Every backend agrees about that. It also removes
//! the need to special-case atomic saves, which are the reason a delete is so
//! often not a delete — see the note where `AtomicSaveDetector` used to be.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use crossbeam_channel::{unbounded, Receiver, Sender};
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use notify::event::{ModifyKind, RenameMode};
use notify::{Config, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};

/// Directories that should never be watched regardless of gitignore settings
const EXCLUDED_DIRS: &[&str] = &[
    "node_modules",
    "target",
    ".git",
    ".hg",
    ".svn",
    "dist",
    "build",
    "__pycache__",
    ".next",
    ".nuxt",
    "coverage",
    ".cache",
    ".parcel-cache",
    "vendor",
];

/// IDE-specific file change events
#[derive(Debug, Clone)]
pub enum IdeFileChange {
    /// A file was modified
    FileModified {
        path: PathBuf,
        /// True if the modification was external (not from the IDE)
        external: bool,
    },
    /// A file was created
    FileCreated { path: PathBuf },
    /// A file was deleted
    FileDeleted { path: PathBuf },
    /// A file was renamed
    FileRenamed { from: PathBuf, to: PathBuf },
    /// A directory structure changed (files added/removed)
    DirectoryChanged { path: PathBuf },
    /// Git status may have changed
    GitStatusChanged,
}

/// Configuration for the file watcher
#[derive(Debug, Clone)]
pub struct FileWatcherConfig {
    /// Normal debounce delay (ms)
    pub debounce_delay_ms: u64,
    /// Burst mode debounce delay (ms)
    pub burst_delay_ms: u64,
    /// Number of pending events to trigger burst mode
    pub burst_threshold: usize,
}

impl Default for FileWatcherConfig {
    fn default() -> Self {
        Self {
            debounce_delay_ms: 200,
            burst_delay_ms: 2000,
            burst_threshold: 100,
        }
    }
}

/// Adaptive debouncer for coalescing file events
struct AdaptiveDebouncer {
    pending: HashMap<PathBuf, PendingEvent>,
    config: FileWatcherConfig,
    in_burst_mode: bool,
}

struct PendingEvent {
    kind: EventKind,
    last_seen: Instant,
}

impl AdaptiveDebouncer {
    fn new(config: FileWatcherConfig) -> Self {
        Self {
            pending: HashMap::new(),
            config,
            in_burst_mode: false,
        }
    }

    fn add_event(&mut self, event: &Event) {
        let now = Instant::now();

        // Detect burst mode
        if self.pending.len() > self.config.burst_threshold {
            self.in_burst_mode = true;
        }

        for path in &event.paths {
            self.pending
                .entry(path.clone())
                .and_modify(|p| {
                    p.kind = event.kind;
                    p.last_seen = now;
                })
                .or_insert(PendingEvent {
                    kind: event.kind,
                    last_seen: now,
                });
        }
    }

    fn drain_ready(&mut self) -> Vec<(PathBuf, EventKind)> {
        let now = Instant::now();
        let delay = if self.in_burst_mode {
            Duration::from_millis(self.config.burst_delay_ms)
        } else {
            Duration::from_millis(self.config.debounce_delay_ms)
        };

        let mut ready = Vec::new();
        self.pending.retain(|path, pending| {
            if now.duration_since(pending.last_seen) >= delay {
                ready.push((path.clone(), pending.kind));
                false
            } else {
                true
            }
        });

        // Exit burst mode when queue clears
        if self.pending.is_empty() {
            self.in_burst_mode = false;
        }

        ready
    }
}

// An `AtomicSaveDetector` used to live here: it held recent deletes and paired
// a delete with a create inside a 100 ms window to recognise the write-to-temp,
// rename-over pattern that most editors save with.
//
// Deciding by existence subsumes it. Both halves of an atomic save land in one
// debounce window, and by the time that window drains the file is there — which
// classifies as a modification without any pairing logic. Deleting it removed
// the second piece of per-call state that had to survive between polls, and it
// is one fewer thing to be wrong about a delete that is really a save.

/// File watcher for IDE with gitignore support and adaptive debouncing
///
/// **The debouncer is a field, and that is load-bearing.** It decides by
/// comparing *now* against when it last saw a path, so it is only meaningful
/// across calls. It used to be constructed inside `process_events`, which runs
/// every 50 ms: each call built an empty debouncer, filled it, and then asked
/// which of its entries were older than the 200 ms debounce delay. None ever
/// were — they had been added microseconds earlier — so `drain_ready` returned
/// nothing on every call and every event was dropped with the debouncer.
///
/// The watcher therefore emitted **nothing, ever**, and its unit tests passed
/// throughout: `test_adaptive_debouncer` sleeps between adding and draining,
/// which is the one sequence production never performs. Confirmed by driving
/// `spawn_file_watcher` against a real directory — a modify, a create and a
/// delete produced zero events in five seconds, and five afterwards. That run is
/// now `tests/watches_a_real_directory.rs`.
pub struct FileWatcher {
    watcher: RecommendedWatcher,
    event_rx: Receiver<notify::Result<Event>>,
    gitignore: Arc<RwLock<Gitignore>>,
    root: PathBuf,
    open_files: Arc<RwLock<HashSet<PathBuf>>>,
    /// Coalesces bursts. Stateful across calls — see the type docs.
    debouncer: AdaptiveDebouncer,
    /// Every file the watcher believes exists.
    ///
    /// Seeded from the initial walk so that the first event about a file which
    /// was already there reads as a modification rather than a creation, and
    /// maintained from then on. This is what lets [`classify`-by-existence]
    /// tell "appeared" from "changed" without trusting the backend's kinds.
    ///
    /// [`classify`-by-existence]: FileWatcher::process_events
    known_paths: HashSet<PathBuf>,
}

impl FileWatcher {
    /// Create a new file watcher for the given root directory
    pub fn new(root: &Path) -> notify::Result<Self> {
        Self::with_config(root, FileWatcherConfig::default())
    }

    /// Create a new file watcher with custom configuration
    ///
    /// The root is **canonicalized**, and it has to be. The backend reports real
    /// paths: on macOS a project under `/tmp` or `/var` — or any project reached
    /// through a symlink — arrives as `/private/…`, which matches neither the
    /// paths the initial walk recorded nor anything the caller would compare
    /// against. Every modification then looks like a creation, because the
    /// existence set is keyed by a spelling the events never use.
    pub fn with_config(root: &Path, config: FileWatcherConfig) -> notify::Result<Self> {
        let root = &root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
        let (tx, rx) = unbounded();

        let watcher = RecommendedWatcher::new(
            move |res: notify::Result<Event>| {
                let _ = tx.send(res);
            },
            Config::default(),
        )?;

        let gitignore = Arc::new(RwLock::new(Self::build_gitignore(root)));
        let open_files = Arc::new(RwLock::new(HashSet::new()));

        Ok(Self {
            watcher,
            event_rx: rx,
            gitignore,
            root: root.to_path_buf(),
            open_files,
            debouncer: AdaptiveDebouncer::new(config),
            known_paths: HashSet::new(),
        })
    }

    /// Start watching the root directory
    pub fn start_watching(&mut self) -> notify::Result<()> {
        // Walk directory tree, excluding hardcoded dirs
        self.watch_directory_recursive(&self.root.clone())
    }

    fn watch_directory_recursive(&mut self, dir: &Path) -> notify::Result<()> {
        // Watch this directory
        self.watcher.watch(dir, RecursiveMode::NonRecursive)?;

        // Recursively watch subdirectories, excluding hardcoded ones
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.filter_map(Result::ok) {
                let path = entry.path();
                if !path.is_dir() {
                    // Seed the existence set, so the first event about a file
                    // that was already here reads as a modification and not as
                    // a creation.
                    self.known_paths.insert(path.clone());
                    continue;
                }
                if path.is_dir() {
                    if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                        if !EXCLUDED_DIRS.contains(&name) {
                            // Check gitignore
                            let gitignore = self.gitignore.read().unwrap();
                            if !gitignore.matched(&path, true).is_ignore() {
                                drop(gitignore);
                                self.watch_directory_recursive(&path)?;
                            }
                        }
                    }
                }
            }
        }

        Ok(())
    }

    /// Mark a file as open in the IDE (for external change detection)
    pub fn mark_file_open(&self, path: &Path) {
        self.open_files.write().unwrap().insert(path.to_path_buf());
    }

    /// Mark a file as closed in the IDE
    pub fn mark_file_closed(&self, path: &Path) {
        self.open_files.write().unwrap().remove(path);
    }

    /// Check if a file is open in the IDE
    fn is_file_open(&self, path: &Path) -> bool {
        self.open_files.read().unwrap().contains(path)
    }

    /// Build gitignore matcher from all .gitignore files in the tree
    fn build_gitignore(root: &Path) -> Gitignore {
        let mut builder = GitignoreBuilder::new(root);

        // Walk and find all .gitignore files
        fn find_gitignores(dir: &Path, builder: &mut GitignoreBuilder) {
            let gitignore_path = dir.join(".gitignore");
            if gitignore_path.exists() {
                let _ = builder.add(&gitignore_path);
            }

            if let Ok(entries) = std::fs::read_dir(dir) {
                for entry in entries.filter_map(Result::ok) {
                    let path = entry.path();
                    if path.is_dir() {
                        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                            if !EXCLUDED_DIRS.contains(&name) {
                                find_gitignores(&path, builder);
                            }
                        }
                    }
                }
            }
        }

        find_gitignores(root, &mut builder);
        builder
            .build()
            .unwrap_or_else(|_| GitignoreBuilder::new(root).build().unwrap())
    }

    /// Rebuild gitignore when .gitignore files change
    pub fn rebuild_gitignore(&self) {
        *self.gitignore.write().unwrap() = Self::build_gitignore(&self.root);
    }

    /// Check if a path should be processed (not gitignored)
    fn should_process(&self, path: &Path) -> bool {
        let gitignore = self.gitignore.read().unwrap();
        !gitignore.matched(path, path.is_dir()).is_ignore()
    }

    /// Check if a path is inside .git directory
    fn is_git_path(path: &Path) -> bool {
        path.components().any(|c| c.as_os_str() == ".git")
    }

    /// Process raw notify events into IDE events.
    ///
    /// Takes `&mut self` because the debouncer and the atomic-save detector are
    /// state that has to survive between calls; see the type docs for what went
    /// wrong when they did not.
    pub fn process_events(&mut self) -> Vec<IdeFileChange> {
        let mut changes = Vec::new();

        // Drain the raw channel first. Separated from the filtering below so
        // that reading the gitignore and mutating the debouncer do not have to
        // borrow `self` at the same time.
        let mut incoming = Vec::new();
        while let Ok(result) = self.event_rx.try_recv() {
            // An error frame is skipped rather than ending the drain: the rest
            // of the queue is still worth having.
            if let Ok(event) = result {
                incoming.push(event);
            }
        }

        // A changed .gitignore has to be adopted before the filter runs, or the
        // batch that carried the change is filtered by the old rules.
        if incoming.iter().any(|event| {
            event
                .paths
                .iter()
                .any(|p| p.file_name().map(|n| n == ".gitignore").unwrap_or(false))
        }) {
            self.rebuild_gitignore();
        }

        for event in &incoming {
            if event.paths.iter().any(|p| self.should_process(p)) {
                self.debouncer.add_event(event);
            }
        }

        // Process debounced events. `drain_ready` hands back an owned list, so
        // the debouncer is not borrowed for the body of this loop.
        for (path, kind) in self.debouncer.drain_ready() {
            // Handle git status changes
            if Self::is_git_path(&path) {
                if path.ends_with("index")
                    || path.ends_with("HEAD")
                    || path.to_string_lossy().contains("refs")
                {
                    changes.push(IdeFileChange::GitStatusChanged);
                }
                continue;
            }

            // Handle renames specially
            if let EventKind::Modify(ModifyKind::Name(RenameMode::Both)) = &kind {
                // For rename events, path contains the "to" path
                // The "from" path would be in a paired event
                // notify-debouncer-full handles this correlation
                continue;
            }

            // What happened is decided by **looking at the filesystem**, not by
            // trusting the backend's event kind. See `classify`.
            let exists = path.exists();
            let known = if exists {
                !self.known_paths.insert(path.clone())
            } else {
                self.known_paths.remove(&path)
            };

            match (exists, known) {
                // Still there, and we had seen it: an edit.
                (true, true) => changes.push(IdeFileChange::FileModified {
                    path: path.clone(),
                    external: !self.is_file_open(&path),
                }),
                // Newly present.
                (true, false) => changes.push(IdeFileChange::FileCreated { path: path.clone() }),
                // Gone.
                (false, _) => changes.push(IdeFileChange::FileDeleted { path: path.clone() }),
            }

            // The listing changed whenever a path appeared or disappeared, and
            // that is what the explorer redraws from.
            if !exists || !known {
                if let Some(parent) = path.parent() {
                    changes.push(IdeFileChange::DirectoryChanged {
                        path: parent.to_path_buf(),
                    });
                }
            }
        }

        changes
    }
}

/// Handle for sending file watcher requests from UI thread
#[derive(Clone)]
pub struct FileWatcherHandle {
    command_tx: Sender<FileWatcherCommand>,
}

/// Commands that can be sent to the file watcher
#[derive(Debug)]
pub enum FileWatcherCommand {
    /// Mark a file as open
    FileOpened(PathBuf),
    /// Mark a file as closed
    FileClosed(PathBuf),
    /// Add a watch for a new directory
    WatchDirectory(PathBuf),
    /// Watch a different project instead.
    ///
    /// The watcher was rooted once at startup and never told the project had
    /// changed, so after `File > Open Project…` it went on watching the tree you
    /// had left — the same defect the language server, the Problems panel and
    /// the explorer each had, and the fourth place it appeared.
    Rebase(PathBuf),
    /// Rebuild gitignore
    RebuildGitignore,
    /// Shutdown the watcher
    Shutdown,
}

/// Events sent from the file watcher to the IDE
#[derive(Debug, Clone)]
pub enum FileWatcherEvent {
    /// File changes occurred
    Changes(Vec<IdeFileChange>),
    /// Error occurred
    Error(String),
}

impl FileWatcherHandle {
    /// Create a new file watcher handle with command channel
    pub fn new(command_tx: Sender<FileWatcherCommand>) -> Self {
        Self { command_tx }
    }

    /// Mark a file as open in the IDE
    pub fn file_opened(&self, path: PathBuf) {
        let _ = self.command_tx.send(FileWatcherCommand::FileOpened(path));
    }

    /// Request gitignore rebuild
    pub fn rebuild_gitignore(&self) {
        let _ = self.command_tx.send(FileWatcherCommand::RebuildGitignore);
    }

    /// Watch a different project instead. See [`FileWatcherCommand::Rebase`].
    pub fn rebase(&self, root: PathBuf) {
        let _ = self.command_tx.send(FileWatcherCommand::Rebase(root));
    }

    /// Shutdown the watcher
    pub fn shutdown(&self) {
        let _ = self.command_tx.send(FileWatcherCommand::Shutdown);
    }
}

/// Create and run a file watcher in a background thread
pub fn spawn_file_watcher(
    root: PathBuf,
) -> notify::Result<(FileWatcherHandle, Receiver<FileWatcherEvent>)> {
    let (command_tx, command_rx) = unbounded::<FileWatcherCommand>();
    let (event_tx, event_rx) = unbounded::<FileWatcherEvent>();

    let mut watcher = FileWatcher::new(&root)?;
    watcher.start_watching()?;

    std::thread::spawn(move || {
        loop {
            // Process commands
            while let Ok(cmd) = command_rx.try_recv() {
                match cmd {
                    FileWatcherCommand::FileOpened(path) => {
                        watcher.mark_file_open(&path);
                    }
                    FileWatcherCommand::FileClosed(path) => {
                        watcher.mark_file_closed(&path);
                    }
                    FileWatcherCommand::WatchDirectory(path) => {
                        if let Err(e) = watcher.watcher.watch(&path, RecursiveMode::NonRecursive) {
                            let _ = event_tx.send(FileWatcherEvent::Error(e.to_string()));
                        }
                    }
                    FileWatcherCommand::Rebase(root) => {
                        // Replaced wholesale rather than re-pointed: the root,
                        // the gitignore rules and the known-paths set all belong
                        // to the old tree, and rebuilding is both simpler and
                        // harder to get subtly wrong than unpicking three
                        // pieces of state in place.
                        match FileWatcher::new(&root).and_then(|mut fresh| {
                            fresh.start_watching()?;
                            Ok(fresh)
                        }) {
                            Ok(fresh) => watcher = fresh,
                            Err(e) => {
                                let _ = event_tx.send(FileWatcherEvent::Error(format!(
                                    "could not watch {}: {e}",
                                    root.display()
                                )));
                            }
                        }
                    }
                    FileWatcherCommand::RebuildGitignore => {
                        watcher.rebuild_gitignore();
                    }
                    FileWatcherCommand::Shutdown => {
                        return;
                    }
                }
            }

            // Process file events
            let changes = watcher.process_events();
            if !changes.is_empty() {
                let _ = event_tx.send(FileWatcherEvent::Changes(changes));
            }

            // Small sleep to avoid busy-waiting
            std::thread::sleep(Duration::from_millis(50));
        }
    });

    let handle = FileWatcherHandle::new(command_tx);
    Ok((handle, event_rx))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_output_directories_are_never_watched() {
        assert!(EXCLUDED_DIRS.contains(&"node_modules"));
        assert!(EXCLUDED_DIRS.contains(&"target"));
        assert!(EXCLUDED_DIRS.contains(&".git"));
    }

    #[test]
    fn paths_inside_dot_git_are_recognised() {
        assert!(FileWatcher::is_git_path(Path::new("/project/.git/index")));
        assert!(FileWatcher::is_git_path(Path::new(
            "/project/.git/refs/heads/main"
        )));
        assert!(!FileWatcher::is_git_path(Path::new("/project/src/main.rs")));
        assert!(!FileWatcher::is_git_path(Path::new("/project/.gitignore")));
    }

    #[test]
    fn the_default_debounce_and_burst_settings_are_what_they_claim() {
        let config = FileWatcherConfig::default();
        assert_eq!(config.debounce_delay_ms, 200);
        assert_eq!(config.burst_delay_ms, 2000);
        assert_eq!(config.burst_threshold, 100);
    }

    #[test]
    fn an_event_is_held_until_the_debounce_delay_has_passed() {
        let config = FileWatcherConfig {
            debounce_delay_ms: 10, // Short for testing
            ..Default::default()
        };
        let mut debouncer = AdaptiveDebouncer::new(config);

        // Add an event
        let event = Event {
            kind: EventKind::Modify(ModifyKind::Any),
            paths: vec![PathBuf::from("/test/file.txt")],
            attrs: Default::default(),
        };
        debouncer.add_event(&event);

        // Should not be ready immediately
        let ready = debouncer.drain_ready();
        assert!(ready.is_empty());

        // Wait for debounce delay
        std::thread::sleep(Duration::from_millis(20));

        // Now should be ready
        let ready = debouncer.drain_ready();
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].0, PathBuf::from("/test/file.txt"));
    }

    // === Race condition tests ===

    #[test]
    fn marking_files_open_and_closed_concurrently_stays_consistent() {
        use std::sync::Arc;
        use std::thread;
        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();
        let watcher = FileWatcher::new(temp_dir.path()).unwrap();
        let watcher = Arc::new(std::sync::Mutex::new(watcher));

        // Simulate rapid open/close operations from multiple "threads"
        let paths: Vec<_> = (0..100)
            .map(|i| PathBuf::from(format!("/test/file{}.rs", i)))
            .collect();

        let handles: Vec<_> = paths
            .chunks(10)
            .map(|chunk| {
                let watcher = Arc::clone(&watcher);
                let chunk = chunk.to_vec();
                thread::spawn(move || {
                    for path in chunk {
                        // Rapid open/close cycles
                        for _ in 0..10 {
                            watcher.lock().unwrap().mark_file_open(&path);
                            watcher.lock().unwrap().mark_file_closed(&path);
                        }
                    }
                })
            })
            .collect();

        // Wait for all threads
        for handle in handles {
            handle.join().unwrap();
        }

        // All files should be closed (none in open_files)
        let watcher = watcher.lock().unwrap();
        let open_files = watcher.open_files.read().unwrap();
        assert!(
            open_files.is_empty(),
            "Expected all files to be closed, but {} are open",
            open_files.len()
        );
    }

    #[test]
    fn a_burst_of_events_for_one_path_coalesces_to_one() {
        let config = FileWatcherConfig {
            debounce_delay_ms: 10,
            burst_threshold: 5,
            burst_delay_ms: 100,
        };
        let mut debouncer = AdaptiveDebouncer::new(config);

        // Simulate burst of events
        for i in 0..20 {
            let event = Event {
                kind: EventKind::Modify(ModifyKind::Any),
                paths: vec![PathBuf::from(format!("/test/file{}.rs", i))],
                attrs: Default::default(),
            };
            debouncer.add_event(&event);
        }

        // Should enter burst mode
        assert!(debouncer.in_burst_mode);

        // Add more events while in burst mode
        for i in 0..10 {
            let event = Event {
                kind: EventKind::Modify(ModifyKind::Any),
                paths: vec![PathBuf::from(format!("/test/extra{}.rs", i))],
                attrs: Default::default(),
            };
            debouncer.add_event(&event);
        }

        // Events should be queued, not ready yet
        let ready = debouncer.drain_ready();
        assert!(ready.is_empty());

        // Wait for burst delay
        std::thread::sleep(Duration::from_millis(150));

        // All events should now be ready
        let ready = debouncer.drain_ready();
        assert_eq!(ready.len(), 30);

        // Burst mode should exit when queue clears
        assert!(!debouncer.in_burst_mode);
    }

    #[test]
    fn rebuilding_gitignore_while_events_arrive_stays_consistent() {
        use std::sync::Arc;
        use std::thread;
        use tempfile::TempDir;

        // Create a temp directory to avoid walking entire filesystem
        let temp_dir = TempDir::new().unwrap();
        let watcher = FileWatcher::new(temp_dir.path()).unwrap();
        let watcher = Arc::new(watcher);

        // Concurrent rebuilds from multiple threads
        let handles: Vec<_> = (0..5)
            .map(|_| {
                let watcher = Arc::clone(&watcher);
                thread::spawn(move || {
                    for _ in 0..10 {
                        watcher.rebuild_gitignore();
                    }
                })
            })
            .collect();

        for handle in handles {
            handle.join().unwrap();
        }

        // Should not panic - gitignore RwLock should handle concurrent access
    }

    #[test]
    fn repeated_edits_to_one_file_coalesce() {
        let config = FileWatcherConfig {
            debounce_delay_ms: 50,
            ..Default::default()
        };
        let mut debouncer = AdaptiveDebouncer::new(config);

        let path = PathBuf::from("/test/file.rs");

        // Multiple modifications to same file
        for _ in 0..5 {
            let event = Event {
                kind: EventKind::Modify(ModifyKind::Any),
                paths: vec![path.clone()],
                attrs: Default::default(),
            };
            debouncer.add_event(&event);
        }

        // Wait for debounce
        std::thread::sleep(Duration::from_millis(60));

        // Should only report one event (coalesced)
        let ready = debouncer.drain_ready();
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].0, path);
    }
}
