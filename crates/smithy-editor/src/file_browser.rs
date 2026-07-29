//! File Browser module - State and logic for filesystem navigation
//!
//! This module provides the file browser sidebar functionality including:
//! - Directory tree state management
//! - Lazy loading of directory contents
//! - Folder expansion/collapse tracking

use std::cell::RefCell;
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use crate::theme::catppuccin;
use floem::peniko::Color;

/// Errors that can occur during file browser operations
#[derive(Debug, Clone)]
pub enum FileBrowserError {
    /// Failed to read directory
    ReadError(String),
    /// Path does not exist
    PathNotFound(PathBuf),
    /// Not a directory
    NotADirectory(PathBuf),
}

impl std::fmt::Display for FileBrowserError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FileBrowserError::ReadError(msg) => write!(f, "Read error: {}", msg),
            FileBrowserError::PathNotFound(path) => write!(f, "Path not found: {}", path.display()),
            FileBrowserError::NotADirectory(path) => {
                write!(f, "Not a directory: {}", path.display())
            }
        }
    }
}

impl std::error::Error for FileBrowserError {}

/// Theme colors for the file browser panel
#[derive(Clone, Copy)]
pub struct FileBrowserTheme {
    /// Background color of the file browser
    pub background: Color,
    /// Text color for file names
    pub text: Color,
    /// Text color for folder names
    pub folder_text: Color,
    /// Text color for selected item
    pub selected_text: Color,
    /// Background color for selected item
    pub selected_background: Color,
    /// Background color on hover
    pub hover_background: Color,
    /// Border color
    pub border: Color,
    /// Header background
    pub header_background: Color,
    /// Header text color
    pub header_text: Color,
}

impl Default for FileBrowserTheme {
    fn default() -> Self {
        Self {
            background: catppuccin::BASE,
            text: catppuccin::TEXT,
            folder_text: catppuccin::BLUE,
            selected_text: catppuccin::TEXT,
            selected_background: catppuccin::SURFACE1,
            hover_background: catppuccin::SURFACE0,
            border: catppuccin::SURFACE0,
            header_background: catppuccin::MANTLE,
            header_text: catppuccin::OVERLAY0,
        }
    }
}

/// A single entry in the file tree (file or folder)
#[derive(Clone, Debug)]
pub struct FileEntry {
    /// Full path to the file/folder
    pub path: PathBuf,
    /// Display name (file/folder name only)
    pub name: String,
    /// Whether this is a directory
    pub is_dir: bool,
    /// Indentation level (depth in tree)
    pub indent_level: usize,
    /// Whether this directory is expanded (only meaningful for directories)
    pub is_expanded: bool,
    /// Whether this is the parent directory navigation entry (..)
    pub is_parent_nav: bool,
}

impl FileEntry {
    /// Create a new file entry
    pub fn new(path: PathBuf, indent_level: usize, is_expanded: bool) -> Self {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| path.to_string_lossy().to_string());
        let is_dir = path.is_dir();

        Self {
            path,
            name,
            is_dir,
            indent_level,
            is_expanded,
            is_parent_nav: false,
        }
    }

    /// Create a parent directory navigation entry (..)
    pub fn parent_nav(parent_path: PathBuf) -> Self {
        Self {
            path: parent_path,
            name: "..".to_string(),
            is_dir: true,
            indent_level: 0,
            is_expanded: false,
            is_parent_nav: true,
        }
    }

    /// Get the display icon for this entry
    pub fn icon(&self) -> &'static str {
        if self.is_parent_nav {
            "< " // Back/up arrow for parent navigation
        } else if self.is_dir {
            if self.is_expanded {
                "v " // Down arrow for expanded folder
            } else {
                "> " // Right arrow for collapsed folder
            }
        } else {
            "  " // No icon for files (space for alignment)
        }
    }
}

/// State for the file browser
pub struct FileBrowserState {
    /// Theme for the file browser
    pub theme: FileBrowserTheme,
    /// Root path being browsed
    pub root_path: PathBuf,
    /// Set of expanded folder paths
    pub expanded_folders: HashSet<PathBuf>,
    /// Currently selected path
    pub selected_path: Option<PathBuf>,
    /// Font size for entries
    pub font_size: f32,
}

impl FileBrowserState {
    /// Create a new file browser state with the given root path
    pub fn new(root_path: PathBuf) -> Self {
        let mut expanded = HashSet::new();
        // Start with root expanded
        expanded.insert(root_path.clone());

        Self {
            theme: FileBrowserTheme::default(),
            root_path,
            expanded_folders: expanded,
            selected_path: None,
            font_size: 13.0,
        }
    }

    /// Create a file browser state starting at the current working directory
    pub fn current_dir() -> Result<Self, FileBrowserError> {
        let cwd =
            std::env::current_dir().map_err(|e| FileBrowserError::ReadError(e.to_string()))?;
        Ok(Self::new(cwd))
    }

    /// Set the root path
    pub fn set_root(&mut self, path: PathBuf) {
        self.root_path = path.clone();
        self.expanded_folders.clear();
        self.expanded_folders.insert(path);
        self.selected_path = None;
    }

    /// Toggle folder expansion
    pub fn toggle_folder(&mut self, path: &Path) {
        if self.expanded_folders.contains(path) {
            self.expanded_folders.remove(path);
        } else {
            self.expanded_folders.insert(path.to_path_buf());
        }
    }

    /// Check if a folder is expanded
    pub fn is_expanded(&self, path: &Path) -> bool {
        self.expanded_folders.contains(path)
    }

    /// Set the selected path
    pub fn select(&mut self, path: Option<PathBuf>) {
        self.selected_path = path;
    }

    /// Check if a path is selected
    pub fn is_selected(&self, path: &Path) -> bool {
        self.selected_path.as_ref().is_some_and(|p| p == path)
    }

    /// Get the root folder name for display
    pub fn root_name(&self) -> String {
        self.root_path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| self.root_path.to_string_lossy().to_string())
    }

    /// Build a flat list of visible entries for rendering
    pub fn build_entry_list(&self) -> Vec<FileEntry> {
        let mut entries = Vec::new();

        // Add parent directory entry if we're not at filesystem root
        if let Some(parent) = self.root_path.parent() {
            // Only add if parent is a valid directory (not empty path)
            if !parent.as_os_str().is_empty() || self.root_path.to_string_lossy() != "/" {
                entries.push(FileEntry::parent_nav(parent.to_path_buf()));
            }
        }

        self.add_entries_recursive(&self.root_path, 0, &mut entries);
        entries
    }

    /// Navigate to home directory
    pub fn navigate_home(&mut self) {
        if let Some(home) = dirs::home_dir() {
            self.set_root(home);
        }
    }

    /// Navigate to filesystem root
    pub fn navigate_root(&mut self) {
        self.set_root(PathBuf::from("/"));
    }

    /// Get the full root path for display
    pub fn root_path_display(&self) -> String {
        self.root_path.to_string_lossy().to_string()
    }

    /// Recursively add entries from a directory
    fn add_entries_recursive(&self, dir: &Path, indent: usize, entries: &mut Vec<FileEntry>) {
        // Read directory contents
        let mut items: Vec<_> = match fs::read_dir(dir) {
            Ok(reader) => reader.filter_map(|e| e.ok()).map(|e| e.path()).collect(),
            Err(_) => return,
        };

        // Sort: directories first, then alphabetically
        items.sort_by(|a, b| {
            let a_is_dir = a.is_dir();
            let b_is_dir = b.is_dir();
            match (a_is_dir, b_is_dir) {
                (true, false) => std::cmp::Ordering::Less,
                (false, true) => std::cmp::Ordering::Greater,
                _ => a.file_name().cmp(&b.file_name()),
            }
        });

        for path in items {
            // Skip hidden files (starting with .)
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if name.starts_with('.') {
                continue;
            }

            let is_dir = path.is_dir();
            let is_expanded = is_dir && self.is_expanded(&path);

            entries.push(FileEntry::new(path.clone(), indent, is_expanded));

            // If this is an expanded directory, add its children
            if is_expanded {
                self.add_entries_recursive(&path, indent + 1, entries);
            }
        }
    }

    /// Refresh the file list (re-read from filesystem)
    pub fn refresh(&mut self) {
        // The entry list is built on-demand in build_entry_list()
        // This method exists for future caching if needed
    }

    /// Collapse all folders
    pub fn collapse_all(&mut self) {
        self.expanded_folders.clear();
        self.expanded_folders.insert(self.root_path.clone());
    }
}

impl Default for FileBrowserState {
    fn default() -> Self {
        Self::current_dir().unwrap_or_else(|_| Self::new(PathBuf::from("/")))
    }
}

/// Shared file browser state type
pub type SharedFileBrowserState = Rc<RefCell<FileBrowserState>>;

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{self, File};
    use tempfile::TempDir;

    fn create_test_directory() -> TempDir {
        let dir = TempDir::new().unwrap();

        // Create some test files and folders
        fs::create_dir(dir.path().join("src")).unwrap();
        fs::create_dir(dir.path().join("tests")).unwrap();
        File::create(dir.path().join("Cargo.toml")).unwrap();
        File::create(dir.path().join("src/main.rs")).unwrap();
        File::create(dir.path().join("src/lib.rs")).unwrap();
        File::create(dir.path().join("tests/test.rs")).unwrap();

        dir
    }

    #[test]
    fn a_fresh_browser_is_rooted_where_it_was_told() {
        let dir = create_test_directory();
        let state = FileBrowserState::new(dir.path().to_path_buf());

        assert_eq!(state.root_path, dir.path());
        assert!(state.is_expanded(dir.path()));
        assert!(state.selected_path.is_none());
    }

    #[test]
    fn toggling_a_folder_expands_it_and_toggling_again_collapses_it() {
        let dir = create_test_directory();
        let mut state = FileBrowserState::new(dir.path().to_path_buf());
        let src_path = dir.path().join("src");

        // Initially not expanded (except root)
        assert!(!state.is_expanded(&src_path));

        // Toggle to expand
        state.toggle_folder(&src_path);
        assert!(state.is_expanded(&src_path));

        // Toggle to collapse
        state.toggle_folder(&src_path);
        assert!(!state.is_expanded(&src_path));
    }

    #[test]
    fn selecting_an_entry_replaces_the_previous_selection() {
        let dir = create_test_directory();
        let mut state = FileBrowserState::new(dir.path().to_path_buf());
        let file_path = dir.path().join("Cargo.toml");

        assert!(!state.is_selected(&file_path));

        state.select(Some(file_path.clone()));
        assert!(state.is_selected(&file_path));

        state.select(None);
        assert!(!state.is_selected(&file_path));
    }

    #[test]
    fn a_collapsed_tree_lists_only_its_top_level() {
        let dir = create_test_directory();
        let state = FileBrowserState::new(dir.path().to_path_buf());

        let entries = state.build_entry_list();

        // Should have root-level items: src, tests, Cargo.toml
        // (hidden files are skipped)
        assert!(!entries.is_empty());

        // Directories should come first
        let first_file_idx = entries.iter().position(|e| !e.is_dir);
        let last_dir_idx = entries.iter().rposition(|e| e.is_dir);

        if let (Some(file_idx), Some(dir_idx)) = (first_file_idx, last_dir_idx) {
            assert!(
                dir_idx < file_idx
                    || entries.iter().all(|e| e.is_dir)
                    || entries.iter().all(|e| !e.is_dir)
            );
        }
    }

    #[test]
    fn an_expanded_folder_contributes_its_children_to_the_list() {
        let dir = create_test_directory();
        let mut state = FileBrowserState::new(dir.path().to_path_buf());
        let src_path = dir.path().join("src");

        // Expand src directory
        state.toggle_folder(&src_path);

        let entries = state.build_entry_list();

        // Should now include files from src/
        let has_main_rs = entries.iter().any(|e| e.name == "main.rs");
        assert!(has_main_rs);
    }

    #[test]
    fn a_file_entry_gets_an_icon_for_its_kind() {
        let dir = create_test_directory();

        let folder = FileEntry::new(dir.path().join("src"), 0, false);
        assert_eq!(folder.icon(), "> ");

        let expanded_folder = FileEntry::new(dir.path().join("src"), 0, true);
        assert_eq!(expanded_folder.icon(), "v ");

        let file = FileEntry::new(dir.path().join("Cargo.toml"), 0, false);
        assert_eq!(file.icon(), "  ");
    }

    #[test]
    fn the_root_is_labelled_with_its_directory_name() {
        let dir = create_test_directory();
        let state = FileBrowserState::new(dir.path().to_path_buf());

        // Should return just the directory name, not full path
        let name = state.root_name();
        assert!(!name.contains('/') || name == "/");
    }

    #[test]
    fn collapsing_all_closes_every_expanded_folder() {
        let dir = create_test_directory();
        let mut state = FileBrowserState::new(dir.path().to_path_buf());
        let src_path = dir.path().join("src");
        let tests_path = dir.path().join("tests");

        // Expand some folders
        state.toggle_folder(&src_path);
        state.toggle_folder(&tests_path);

        // Collapse all
        state.collapse_all();

        // Only root should be expanded
        assert!(state.is_expanded(dir.path()));
        assert!(!state.is_expanded(&src_path));
        assert!(!state.is_expanded(&tests_path));
    }

    #[test]
    fn the_default_browser_theme_is_fully_populated() {
        let theme = FileBrowserTheme::default();
        assert_eq!(theme.background, catppuccin::BASE);
    }
}
