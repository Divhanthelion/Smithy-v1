//! Loading a file into memory, and the metadata the UI needs to describe it.
//!
//! **Explicitly not an editing model.** Text mutation, undo/redo and cursor
//! state all live in floem's editor document, which owns the text you type into
//! — see [`crate::code_editor`]. Keeping a second, parallel editing model here
//! is what previously let the tab bar and the editor disagree about whether a
//! file was dirty.
//!
//! What was left of that second model has been removed: a dirty flag nothing
//! set, a path setter nothing called, an async atomic `save` superseded by
//! `EditorHandle::save`, and a `LineEnding` that was detected on load and never
//! read by anything — including the save it existed for.

use ropey::Rope;

use std::path::{Path, PathBuf};

use crate::error::BufferError;

/// Unique identifier for a buffer
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct BufferId(u64);

impl BufferId {
    /// Create a new unique buffer ID
    pub fn new() -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        BufferId(COUNTER.fetch_add(1, Ordering::SeqCst))
    }
}

impl Default for BufferId {
    fn default() -> Self {
        Self::new()
    }
}

/// A file loaded into memory, with the metadata the UI needs to describe it.
///
/// Deliberately **not** an editing model. Text mutation, undo/redo and cursor
/// state all live in floem's editor document, which owns the text you actually
/// type into. Keeping a second, parallel editing model here is what previously
/// let the tab bar and the editor disagree about whether a file was dirty.
pub struct Buffer {
    id: BufferId,
    text: Rope,
    path: Option<PathBuf>,
    language_id: Option<String>,
}

impl Buffer {
    /// Create a new empty buffer
    pub fn new() -> Self {
        Self {
            id: BufferId::new(),
            text: Rope::new(),
            path: None,
            language_id: None,
        }
    }

    /// Create a buffer from a string
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(text: &str) -> Self {
        Self {
            id: BufferId::new(),
            text: Rope::from_str(text),
            path: None,
            language_id: None,
        }
    }

    /// Get the buffer's unique ID
    pub fn id(&self) -> BufferId {
        self.id
    }

    /// Get a reference to the underlying rope
    pub fn text(&self) -> &Rope {
        &self.text
    }

    pub fn char_count(&self) -> usize {
        self.text.len_chars()
    }

    /// Get the file path associated with this buffer
    pub fn path(&self) -> Option<&PathBuf> {
        self.path.as_ref()
    }

    /// Get the language ID for syntax highlighting
    pub fn language_id(&self) -> Option<&str> {
        self.language_id.as_deref()
    }

    /// A named buffer that is not a file. Inspection uses this so clicking a
    /// ledger row does not create a Project file or start LSP.
    pub fn scratch(path: PathBuf, text: &str) -> Self {
        let language_id = language_from_path(&path);
        Self {
            id: BufferId::new(),
            text: Rope::from_str(text),
            path: Some(path),
            language_id,
        }
    }

    /// Replace the text. Used when re-clicking an inspection tab.
    pub fn replace_text(&mut self, text: &str) {
        self.text = Rope::from_str(text);
    }

    /// Load a buffer from a file
    ///
    /// # Arguments
    /// * `path` - Path to the file to load
    ///
    /// # Returns
    /// * `Ok(Buffer)` with the file contents
    /// * `Err(BufferError)` if the file cannot be read
    pub fn from_file(path: &Path) -> Result<Self, BufferError> {
        let text = Self::read_rope_from_path(path)?;

        let language_id = language_from_path(path);

        Ok(Self {
            id: BufferId::new(),
            text,
            path: Some(path.to_path_buf()),
            language_id,
        })
    }

    /// Reload the buffer content from disk
    pub fn reload(&mut self) -> Result<(), BufferError> {
        if let Some(path) = &self.path {
            let path_clone = path.clone();
            let text = Self::read_rope_from_path(&path_clone)?;
            self.text = text;
            Ok(())
        } else {
            Err(BufferError::NoFilePath)
        }
    }

    /// Read file content into a Rope
    fn read_rope_from_path(path: &Path) -> Result<Rope, BufferError> {
        use std::fs::File;
        use std::io::BufReader;

        let file = File::open(path).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                BufferError::FileNotFound(path.to_path_buf())
            } else if e.kind() == std::io::ErrorKind::PermissionDenied {
                BufferError::PermissionDenied(path.to_path_buf())
            } else {
                BufferError::Io(e)
            }
        })?;

        let reader = BufReader::new(file);
        Rope::from_reader(reader).map_err(|e| {
            if e.kind() == std::io::ErrorKind::InvalidData {
                BufferError::InvalidUtf8(path.to_path_buf())
            } else {
                BufferError::Io(e)
            }
        })
    }
}

impl Default for Buffer {
    fn default() -> Self {
        Self::new()
    }
}

fn language_from_path(path: &Path) -> Option<String> {
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| match ext {
            "rs" => "rust",
            "py" => "python",
            "js" => "javascript",
            "ts" => "typescript",
            "c" => "c",
            "cpp" | "cc" | "cxx" => "cpp",
            "h" | "hpp" => "cpp",
            "go" => "go",
            "java" => "java",
            "md" => "markdown",
            "toml" => "toml",
            "json" => "json",
            "yaml" | "yml" => "yaml",
            _ => ext,
        })
        .map(String::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    // **Feature: forge-foundation, Property 1: Buffer Serialization Round-Trip**
    // *For any* valid UTF-8 text content, writing the buffer to disk and reading it back
    // into a new buffer SHALL produce identical text content.
    // **Validates: Requirements 2.5, 2.6, 2.7**
    #[test]
    fn prop_buffer_serialization_roundtrip() {
        use proptest::test_runner::{Config, TestRunner};
        use std::io::Write;
        use std::sync::atomic::{AtomicU64, Ordering};

        static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

        let config = Config::with_cases(100);
        let mut runner = TestRunner::new(config);

        // Strategy for generating valid UTF-8 text
        let text_strategy = "\\PC{0,1000}";

        runner
            .run(&text_strategy, |original_text| {
                // Create a unique temp file for this test iteration
                let test_id = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
                let temp_dir = std::env::temp_dir();
                let temp_path = temp_dir.join(format!("forge_test_roundtrip_{}.txt", test_id));

                // Create buffer with original text
                let buffer = Buffer::from_str(&original_text);

                // Write to disk synchronously (simulating what save() does)
                {
                    let mut file = std::fs::File::create(&temp_path).map_err(|e| {
                        proptest::test_runner::TestCaseError::fail(format!("Create error: {}", e))
                    })?;
                    for chunk in buffer.text().chunks() {
                        file.write_all(chunk.as_bytes()).map_err(|e| {
                            proptest::test_runner::TestCaseError::fail(format!(
                                "Write error: {}",
                                e
                            ))
                        })?;
                    }
                    file.flush().map_err(|e| {
                        proptest::test_runner::TestCaseError::fail(format!("Flush error: {}", e))
                    })?;
                }

                // Load from disk
                let loaded_buffer = Buffer::from_file(&temp_path).map_err(|e| {
                    proptest::test_runner::TestCaseError::fail(format!("Load error: {}", e))
                })?;

                // Clean up temp file
                let _ = std::fs::remove_file(&temp_path);

                // Compare content
                let original_content: String = buffer.text().chars().collect();
                let loaded_content: String = loaded_buffer.text().chars().collect();

                prop_assert_eq!(
                    original_content,
                    loaded_content,
                    "Round-trip content mismatch"
                );

                Ok(())
            })
            .unwrap();
    }
}
