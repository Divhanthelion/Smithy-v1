//! Backend implementations of [`crate::Provider`].
//!
//! Supports local endpoints (LM Studio) and cloud endpoints (OpenRouter).

use std::sync::Arc;

use crate::provider::{Provider, ProviderError};

pub mod lmstudio;
pub mod openrouter;
pub mod sse;

pub use lmstudio::{LmStudio, ModelInfo};
pub use openrouter::OpenRouter;

/// Create a provider based on environment configuration.
///
/// Selection precedence:
/// 1. `SMITHY_PROVIDER` or `PROVIDER` env var (`"openrouter"` or `"lmstudio"`).
/// 2. If unset, checks if `OPENROUTER_API_KEY` is present. If present, defaults to `"openrouter"`.
/// 3. Otherwise defaults to `"lmstudio"`.
pub fn create_provider_from_env() -> Result<Arc<dyn Provider>, ProviderError> {
    load_dotenv_if_present();
    let provider_choice = std::env::var("SMITHY_PROVIDER")
        .or_else(|_| std::env::var("PROVIDER"))
        .ok();

    let choice = match provider_choice.as_deref() {
        Some(c) => c.to_lowercase(),
        None => {
            if std::env::var("OPENROUTER_API_KEY")
                .map(|k| !k.trim().is_empty())
                .unwrap_or(false)
            {
                "openrouter".to_string()
            } else {
                "lmstudio".to_string()
            }
        }
    };

    match choice.as_str() {
        "openrouter" => Ok(Arc::new(OpenRouter::from_env()?)),
        "lmstudio" => Ok(Arc::new(LmStudio::from_env()?)),
        other => Err(ProviderError::Other(format!(
            "Unknown provider `{other}` configured in SMITHY_PROVIDER. Supported providers are `openrouter` and `lmstudio`."
        ))),
    }
}

fn load_dotenv_if_present() {
    if let Ok(content) = std::fs::read_to_string(".env") {
        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some((key, val)) = line.split_once('=') {
                let key = key.trim();
                let val = val.trim().trim_matches('"').trim_matches('\'');
                if std::env::var(key).is_err() {
                    std::env::set_var(key, val);
                }
            }
        }
    }
}
