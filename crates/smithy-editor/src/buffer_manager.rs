//! Buffer Manager module - Multi-buffer handling for Smithy
//!
//! This module provides the `BufferManager` struct which manages all open text buffers,
//! including creation, opening, closing, and tracking the active buffer.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use crate::buffer::{Buffer, BufferId};
use crate::error::BufferError;

/// Manages all open text buffers in the editor
pub struct BufferManager {
    /// All open buffers indexed by their ID
    buffers: HashMap<BufferId, Rc<RefCell<Buffer>>>,
    /// The order they were opened in, which is the order the tab bar shows.
    ///
    /// Kept beside the map because closing a tab has to pick a *neighbouring*
    /// one to focus, and a `HashMap` has no neighbours: the previous code took
    /// `buffers.keys().next()`, so which tab you landed on after a close was
    /// whatever the hash order happened to be — different between runs, and
    /// unrelated to the tab you closed. Its test asserted the right answer only
    /// because it used two buffers, where every order gives the same one.
    order: Vec<BufferId>,
    /// The currently active buffer ID
    active: Option<BufferId>,
    /// Map from file paths to buffer IDs for quick lookup
    path_to_buffer: HashMap<PathBuf, BufferId>,
}

impl BufferManager {
    /// Create a new empty buffer manager
    pub fn new() -> Self {
        Self {
            buffers: HashMap::new(),
            order: Vec::new(),
            active: None,
            path_to_buffer: HashMap::new(),
        }
    }

    /// Create a new empty buffer and return its ID
    ///
    /// The new buffer is added to the manager but not set as active.
    pub fn create_buffer(&mut self) -> BufferId {
        let buffer = Buffer::new();
        let id = buffer.id();
        self.buffers.insert(id, Rc::new(RefCell::new(buffer)));
        self.order.push(id);
        id
    }

    /// Open a file and create a buffer for it
    pub fn open_file(&mut self, path: &Path) -> Result<BufferId, BufferError> {
        // Canonicalize the path for consistent lookup
        let canonical_path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());

        // Check if file is already open
        if let Some(&existing_id) = self.path_to_buffer.get(&canonical_path) {
            // Verify the buffer still exists
            if self.buffers.contains_key(&existing_id) {
                return Ok(existing_id);
            }
            // Buffer was removed, clean up the path mapping
            self.path_to_buffer.remove(&canonical_path);
        }

        // Load the file into a new buffer
        let buffer = Buffer::from_file(path)?;
        let id = buffer.id();

        // Register the path mapping
        if let Some(buffer_path) = buffer.path() {
            let canonical = buffer_path
                .canonicalize()
                .unwrap_or_else(|_| buffer_path.clone());
            self.path_to_buffer.insert(canonical, id);
        }

        self.buffers.insert(id, Rc::new(RefCell::new(buffer)));
        self.order.push(id);
        Ok(id)
    }

    /// Get a reference to a buffer by ID
    pub fn get_buffer(&self, id: BufferId) -> Option<Rc<RefCell<Buffer>>> {
        self.buffers.get(&id).cloned()
    }

    /// Get the currently active buffer ID
    pub fn active_id(&self) -> Option<BufferId> {
        self.active
    }

    /// Get a reference to the active buffer
    pub fn active_buffer(&self) -> Option<Rc<RefCell<Buffer>>> {
        self.active.and_then(|id| self.buffers.get(&id).cloned())
    }

    /// Set the active buffer
    pub fn set_active(&mut self, id: Option<BufferId>) -> bool {
        match id {
            Some(buffer_id) => {
                if self.buffers.contains_key(&buffer_id) {
                    self.active = Some(buffer_id);
                    true
                } else {
                    false
                }
            }
            None => {
                self.active = None;
                true
            }
        }
    }

    /// Force close a buffer, discarding any unsaved changes
    pub fn force_close_buffer(&mut self, id: BufferId) -> Option<Buffer> {
        let buffer_rc = self.buffers.remove(&id)?;

        // Clean up path mapping
        // We need to borrow the buffer to get the path
        // Since we removed the Rc from the map, this might be the last reference
        // unless EditorViewState holds one.
        // If EditorViewState holds one, we can still access it.

        // Note: returning Buffer might be hard if there are other references.
        // We'll try to unwrap, otherwise clone if we can, or just return what we can.
        // But Buffer isn't cloneable.

        let buffer_ref = buffer_rc.borrow();
        if let Some(path) = buffer_ref.path() {
            let canonical = path.canonicalize().unwrap_or_else(|_| path.clone());
            self.path_to_buffer.remove(&canonical);
        }
        drop(buffer_ref);

        // Closing the active tab focuses its neighbour — the one to the right,
        // or the one to the left when it was last. Anything else means the
        // focus jumps somewhere unrelated to what you just closed.
        let position = self.order.iter().position(|&b| b == id);
        if let Some(index) = position {
            self.order.remove(index);
            if self.active == Some(id) {
                self.active = self.order.get(index).or_else(|| self.order.last()).copied();
            }
        }

        // Try to unwrap the Rc. If failed (other refs exist), we can't return the Buffer.
        // This changes the signature effectively, or we return None if still shared.
        Rc::try_unwrap(buffer_rc).ok().map(|cell| cell.into_inner())
    }

    /// Every open buffer, in the order they were opened.
    ///
    /// Order matters: the tab bar is built from this, and a `HashMap`'s
    /// iteration order would reshuffle the tabs on every rebuild.
    pub fn buffer_ids(&self) -> impl Iterator<Item = BufferId> + '_ {
        self.order.iter().copied()
    }

    /// Get the number of open buffers
    pub fn buffer_count(&self) -> usize {
        self.buffers.len()
    }

    /// Check if a buffer has unsaved changes
    pub fn is_buffer_dirty(&self, id: BufferId) -> bool {
        self.buffers.get(&id).is_some_and(|b| b.borrow().is_dirty())
    }

    /// Get a buffer ID by file path
    pub fn get_buffer_by_path(&self, path: &Path) -> Option<BufferId> {
        let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        self.path_to_buffer.get(&canonical).copied()
    }
}

impl Default for BufferManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_manager_holds_nothing_and_focuses_nothing() {
        let manager = BufferManager::new();
        assert_eq!(manager.buffer_count(), 0);
        assert!(manager.active_id().is_none());
    }

    #[test]
    fn a_created_buffer_is_retrievable_and_starts_clean() {
        let mut manager = BufferManager::new();
        let id = manager.create_buffer();

        assert_eq!(manager.buffer_count(), 1);
        assert!(manager.get_buffer(id).is_some());
        assert!(!manager.is_buffer_dirty(id));
    }

    #[test]
    fn focus_can_be_moved_and_cleared() {
        let mut manager = BufferManager::new();
        let id = manager.create_buffer();

        // Initially no active buffer
        assert!(manager.active_id().is_none());

        // Set active
        assert!(manager.set_active(Some(id)));
        assert_eq!(manager.active_id(), Some(id));

        // Clear active
        assert!(manager.set_active(None));
        assert!(manager.active_id().is_none());
    }

    #[test]
    fn focusing_a_buffer_that_is_not_open_is_refused() {
        let mut manager = BufferManager::new();
        let fake_id = BufferId::new();

        // Should fail for non-existent buffer
        assert!(!manager.set_active(Some(fake_id)));
        assert!(manager.active_id().is_none());
    }

    /// **Closing a tab focuses its neighbour**, not an arbitrary other tab.
    ///
    /// This needs three buffers to say anything: with two, every possible rule
    /// — first, last, next, previous — picks the same one, which is why the
    /// previous version passed over a `HashMap` whose iteration order made the
    /// answer differ between runs.
    #[test]
    fn closing_the_active_tab_focuses_the_one_beside_it() {
        let mut manager = BufferManager::new();
        let (first, middle, last) = (
            manager.create_buffer(),
            manager.create_buffer(),
            manager.create_buffer(),
        );

        manager.set_active(Some(middle));
        manager.force_close_buffer(middle);
        assert_eq!(
            manager.active_id(),
            Some(last),
            "closing a middle tab should focus the one to its right"
        );

        manager.force_close_buffer(last);
        assert_eq!(
            manager.active_id(),
            Some(first),
            "closing the last tab should fall back to the one on its left"
        );
    }

    /// Closing a tab that is not focused must not move the focus.
    #[test]
    fn closing_an_inactive_tab_leaves_the_focus_alone() {
        let mut manager = BufferManager::new();
        let (first, second) = (manager.create_buffer(), manager.create_buffer());
        manager.set_active(Some(second));
        manager.force_close_buffer(first);
        assert_eq!(manager.active_id(), Some(second));
    }

    /// Tabs keep the order they were opened in, so the bar does not reshuffle
    /// itself every time it is rebuilt.
    #[test]
    fn tabs_stay_in_the_order_they_were_opened() {
        let mut manager = BufferManager::new();
        let opened: Vec<_> = (0..6).map(|_| manager.create_buffer()).collect();
        assert_eq!(manager.buffer_ids().collect::<Vec<_>>(), opened);
    }

    #[test]
    fn every_open_buffer_is_listed() {
        let mut manager = BufferManager::new();
        let id1 = manager.create_buffer();
        let id2 = manager.create_buffer();
        let id3 = manager.create_buffer();

        let ids: Vec<_> = manager.buffer_ids().collect();
        assert_eq!(ids.len(), 3);
        assert!(ids.contains(&id1));
        assert!(ids.contains(&id2));
        assert!(ids.contains(&id3));
    }

    #[tokio::test]
    async fn opening_the_same_file_twice_reuses_one_buffer() {
        use std::io::Write;

        let mut manager = BufferManager::new();

        // Create a temp file
        let temp_dir = std::env::temp_dir();
        let temp_path = temp_dir.join("forge_test_open.txt");
        {
            let mut file = std::fs::File::create(&temp_path).unwrap();
            file.write_all(b"Test content").unwrap();
        }

        // Open the file
        let id = manager.open_file(&temp_path).unwrap();
        assert_eq!(manager.buffer_count(), 1);

        let buffer = manager.get_buffer(id).unwrap();
        assert_eq!(buffer.borrow().text().to_string(), "Test content");
        assert!(!buffer.borrow().is_dirty());

        // Opening same file again should return same buffer
        let id2 = manager.open_file(&temp_path).unwrap();
        assert_eq!(id, id2);
        assert_eq!(manager.buffer_count(), 1);

        // Clean up
        std::fs::remove_file(&temp_path).unwrap();
    }

    #[tokio::test]
    async fn opening_a_file_that_is_not_there_reports_rather_than_panics() {
        let mut manager = BufferManager::new();
        let result = manager.open_file(Path::new("/nonexistent/path/file.txt"));
        assert!(result.is_err());
    }

    #[test]
    fn a_buffer_can_be_found_again_by_its_path() {
        use std::io::Write;

        let mut manager = BufferManager::new();

        // Create a temp file
        let temp_dir = std::env::temp_dir();
        let temp_path = temp_dir.join("forge_test_path_lookup.txt");
        {
            let mut file = std::fs::File::create(&temp_path).unwrap();
            file.write_all(b"Test").unwrap();
        }

        // Open the file
        let id = manager.open_file(&temp_path).unwrap();

        // Look up by path
        let found_id = manager.get_buffer_by_path(&temp_path);
        assert_eq!(found_id, Some(id));

        // Non-existent path
        let not_found = manager.get_buffer_by_path(Path::new("/not/a/real/path.txt"));
        assert!(not_found.is_none());

        // Clean up
        std::fs::remove_file(&temp_path).unwrap();
    }

    #[test]
    fn closing_a_buffer_releases_its_path_so_it_can_reopen() {
        use std::io::Write;

        let mut manager = BufferManager::new();

        // Create a temp file
        let temp_dir = std::env::temp_dir();
        let temp_path = temp_dir.join("forge_test_close_path.txt");
        {
            let mut file = std::fs::File::create(&temp_path).unwrap();
            file.write_all(b"Test").unwrap();
        }

        // Open the file
        let id = manager.open_file(&temp_path).unwrap();
        assert!(manager.get_buffer_by_path(&temp_path).is_some());

        // Close the buffer
        manager.force_close_buffer(id);

        // Path mapping should be removed
        assert!(manager.get_buffer_by_path(&temp_path).is_none());

        // Clean up
        std::fs::remove_file(&temp_path).unwrap();
    }
}

/// A buffer as the tab bar needs to render it.
///
/// Lives here rather than in a separate application-state module: it
/// describes a managed buffer, and the module that previously held it existed
/// only to carry a duplicate app state nothing ever read.
/// Buffer state for UI display
#[derive(Clone, Debug)]
pub struct BufferState {
    /// Buffer ID
    pub id: BufferId,
    /// Display name (filename or "Untitled")
    pub name: String,
    /// Whether the buffer has unsaved changes
    pub is_dirty: bool,
    /// Full file path if available
    pub path: Option<String>,
}
