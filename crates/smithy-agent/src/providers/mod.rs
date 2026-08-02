//! Backend implementations of [`crate::Provider`].
//!
//! Supports local endpoints (LM Studio) and cloud endpoints (OpenRouter).
//!
//! **Selection does not live here.** It lives in [`crate::config`], which reads
//! a settings file the UI writes and falls back to the environment when there is
//! none. This module is only the transports.

use std::sync::Arc;

use crate::config::AgentConfig;
use crate::provider::{Provider, ProviderError};

pub mod deepseek;
pub mod lmstudio;
pub mod openrouter;
pub mod sse;

pub use deepseek::DeepSeek;
pub use lmstudio::{LmStudio, ModelInfo};
pub use openrouter::OpenRouter;

/// Create a provider from the stored settings, falling back to the environment.
///
/// Kept as a free function with the old name because it is what the app calls
/// and what a caller without a data directory needs. Everything it used to do
/// inline — provider precedence, the `.env` parser — now lives in
/// [`crate::config`]; this is the no-data-directory entry point into it.
pub fn create_provider_from_env() -> Result<Arc<dyn Provider>, ProviderError> {
    AgentConfig::from_env().build_provider()
}

/// Create a provider from the settings stored under `data_dir`.
///
/// The path the app takes. Falls back to the environment when nothing has been
/// saved yet, so an installation that has never opened the settings dialog is
/// indistinguishable from one running the previous version.
pub fn create_provider(data_dir: &std::path::Path) -> Result<Arc<dyn Provider>, ProviderError> {
    AgentConfig::load(data_dir).build_provider()
}
