//! LSP (Language Server Protocol) client implementation
//!
//! This module provides a client for communicating with language servers
//! to provide code intelligence features like hover, completions, and diagnostics.

mod client;
mod integration;
mod registry;
mod transport;
mod types;
mod uri;

pub use client::{LspClient, LspClientConfig, LspError, ServerHealth};
pub use integration::{LspHandle, LspManager, LspRequest, LspResponse};
pub use registry::{
    rust_initialization_options, LanguageServerConfig, LspRegistry, ServerAvailability, ServerKey,
    SharedLspRegistry,
};
pub use transport::LspTransport;
pub use types::{
    DocumentPosition, DocumentRange, LspDiagnostic, LspHover,
    PositionEncoding, PositionError, Severity,
};
