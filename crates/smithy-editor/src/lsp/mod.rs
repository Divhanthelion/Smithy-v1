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

pub use client::{
    ClientCrash, ClientDiagnostics, LspClient, LspClientConfig, LspError, LspStamp, ServerHealth,
};
pub use integration::{LspHandle, LspManager, LspRequest, LspResponse};
pub use registry::{
    rust_initialization_options, LanguageServerConfig, LspRegistry, ServerAvailability, ServerKey,
    RestartOutcome, SharedLspRegistry,
};
pub use transport::LspTransport;
pub use types::{
    CompletionKind, DocumentPosition, DocumentRange, LspCompletion, LspDiagnostic, LspHover,
    PositionEncoding, PositionError, Severity,
};
