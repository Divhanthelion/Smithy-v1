//! Error types for the editor.

use std::path::PathBuf;
use thiserror::Error;

/// Errors that can occur during buffer operations
#[derive(Debug, Error)]
pub enum BufferError {
    #[error("File not found: {0}")]
    FileNotFound(PathBuf),

    #[error("Permission denied: {0}")]
    PermissionDenied(PathBuf),

    #[error("Invalid UTF-8 encoding in file: {0}")]
    InvalidUtf8(PathBuf),

    #[error("Position {0} out of bounds (buffer length: {1})")]
    PositionOutOfBounds(usize, usize),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Buffer has no associated file path")]
    NoFilePath,
}

/// Errors that can occur during terminal operations
#[derive(Debug, Error)]
pub enum TerminalError {
    #[error("Failed to spawn PTY: {0}")]
    PtySpawnFailed(String),

    #[error("Shell not found: {0}")]
    ShellNotFound(String),

    #[error("Terminal already closed")]
    AlreadyClosed,

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

/// Errors that can occur during syntax highlighting
#[derive(Debug, Error)]
pub enum HighlightError {
    #[error("Language not supported: {0}")]
    UnsupportedLanguage(String),

    #[error("Parse error: {0}")]
    ParseError(String),
}
