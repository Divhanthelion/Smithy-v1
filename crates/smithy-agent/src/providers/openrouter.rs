//! OpenRouter provider implementation.
//!
//! Connects to OpenRouter's OpenAI-compatible API (`https://openrouter.ai/api/v1`)
//! authenticated via `OPENROUTER_API_KEY`.

use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::provider::{Completion, CompletionRequest, Delta, Provider, ProviderError, Sampling};
use crate::providers::lmstudio::ModelInfo;
use crate::providers::sse::consume_sse_stream;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(900);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

pub struct OpenRouter {
    http: reqwest::Client,
    base_url: String,
    model: String,
    api_key: String,
}

impl OpenRouter {
    pub fn new(
        base_url: impl Into<String>,
        model: impl Into<String>,
        api_key: impl Into<String>,
    ) -> Result<Self, ProviderError> {
        let http = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|e| ProviderError::Other(format!("could not build HTTP client: {e}")))?;

        let api_key = api_key.into();
        let base_url = base_url.into().trim_end_matches('/').to_string();
        let model = model.into();

        Ok(OpenRouter {
            http,
            base_url,
            model,
            api_key,
        })
    }

    /// Read configuration from environment variables:
    /// - `OPENROUTER_API_KEY` (required)
    /// - `OPENROUTER_MODEL` (defaults to `"anthropic/claude-3.5-sonnet"`)
    /// - `OPENROUTER_URL` (defaults to `"https://openrouter.ai/api/v1"`)
    pub fn from_env() -> Result<Self, ProviderError> {
        let api_key = std::env::var("OPENROUTER_API_KEY").map_err(|_| {
            ProviderError::Other(
                "OPENROUTER_API_KEY environment variable is not set. \
                 Set OPENROUTER_API_KEY=<key> to use OpenRouter."
                    .to_string(),
            )
        })?;

        if api_key.trim().is_empty() {
            return Err(ProviderError::Other(
                "OPENROUTER_API_KEY environment variable is empty.".to_string(),
            ));
        }

        let base_url = std::env::var("OPENROUTER_URL")
            .unwrap_or_else(|_| "https://openrouter.ai/api/v1".to_string());
        let model = std::env::var("OPENROUTER_MODEL")
            .unwrap_or_else(|_| "anthropic/claude-3.5-sonnet".to_string());

        OpenRouter::new(base_url, model, api_key)
    }

    fn chat_url(&self) -> String {
        format!("{}/chat/completions", self.base_url)
    }

    fn models_url(&self) -> String {
        format!("{}/models", self.base_url)
    }

    fn auth_key_url(&self) -> String {
        format!("{}/auth/key", self.base_url)
    }

    /// Probe OpenRouter `/models` endpoint to retrieve model context window & info.
    pub async fn probe_model(&self) -> Result<Option<ModelInfo>, ProviderError> {
        let mut req = self.http.get(self.models_url());
        if !self.api_key.is_empty() {
            req = req.bearer_auth(&self.api_key);
        }

        let Ok(response) = req.send().await else {
            return Ok(None);
        };
        if !response.status().is_success() {
            return Ok(None);
        }
        let Ok(text) = response.text().await else {
            return Ok(None);
        };
        let Ok(value) = serde_json::from_str::<Value>(&text) else {
            return Ok(None);
        };
        let Some(data) = value["data"].as_array() else {
            return Ok(None);
        };

        let target = self.model.to_lowercase();
        let entry = data.iter().find(|m| {
            m["id"]
                .as_str()
                .map(|id| id.to_lowercase() == target)
                .unwrap_or(false)
        });

        match entry {
            Some(m) => {
                let ctx_len = m["context_length"].as_i64();
                Ok(Some(ModelInfo {
                    key: self.model.clone(),
                    found: true,
                    loaded: true,
                    context_length: ctx_len,
                    max_context_length: ctx_len,
                    trained_for_tool_use: true,
                    format: "cloud".to_string(),
                    quantization: "api".to_string(),
                }))
            }
            None => Ok(Some(ModelInfo {
                key: self.model.clone(),
                found: true,
                loaded: true,
                context_length: None,
                max_context_length: None,
                trained_for_tool_use: true,
                format: "cloud".to_string(),
                quantization: "api".to_string(),
            })),
        }
    }

    fn build_body(&self, request: &CompletionRequest<'_>) -> Value {
        let s: &Sampling = request.sampling;
        let mut body = json!({
            "model": self.model,
            "messages": request.history.to_api_with_reasoning(false),
            "tools": request.tools,
            "stream": true,
            "temperature": s.temperature,
            "top_p": s.top_p,
            "max_tokens": s.max_tokens,
        });
        body["stream_options"] = json!({ "include_usage": true });
        body
    }
}

#[async_trait]
impl Provider for OpenRouter {
    fn name(&self) -> &str {
        "openrouter"
    }

    /// Without this override the trait's default answers `Ok(None)` and the
    /// caller falls back to the conservative default ceilings — the exact
    /// failure this probe exists to prevent. (Inherent methods do not
    /// dispatch through `dyn Provider`.)
    async fn probe_model(&self) -> Result<Option<ModelInfo>, ProviderError> {
        OpenRouter::probe_model(self).await
    }

    fn model(&self) -> &str {
        &self.model
    }

    fn stub_superseded_snapshots(&self) -> bool {
        true
    }

    async fn preflight(&self) -> Result<(), ProviderError> {
        if self.api_key.trim().is_empty() {
            return Err(ProviderError::Other(
                "OPENROUTER_API_KEY environment variable is empty.".to_string(),
            ));
        }

        let response = self
            .http
            .get(self.auth_key_url())
            .bearer_auth(&self.api_key)
            .header("HTTP-Referer", "https://github.com/finalsmithy")
            .header("X-Title", "Smithy")
            .send()
            .await
            .map_err(|e| ProviderError::Unreachable {
                endpoint: self.auth_key_url(),
                source: Box::new(e),
            })?;

        let status = response.status();
        if status.as_u16() == 401 || status.as_u16() == 403 {
            let body = response.text().await.unwrap_or_default();
            return Err(ProviderError::Http {
                status: status.as_u16(),
                body: format!("Authentication failed for OpenRouter: {body}"),
            });
        }

        Ok(())
    }

    async fn complete(
        &self,
        request: CompletionRequest<'_>,
        on_delta: Option<&(dyn Fn(Delta) + Send + Sync)>,
    ) -> Result<Completion, ProviderError> {
        let response = self
            .http
            .post(self.chat_url())
            .bearer_auth(&self.api_key)
            .header("HTTP-Referer", "https://github.com/finalsmithy")
            .header("X-Title", "Smithy")
            .json(&self.build_body(&request))
            .timeout(request.http_timeout(REQUEST_TIMEOUT))
            .send()
            .await
            .map_err(|e| ProviderError::Unreachable {
                endpoint: self.chat_url(),
                source: Box::new(e),
            })?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(ProviderError::Http {
                status: status.as_u16(),
                body: truncate(&body, 500),
            });
        }

        consume_sse_stream(response.bytes_stream(), on_delta).await
    }

    fn build_body(&self, request: &CompletionRequest<'_>) -> Value {
        OpenRouter::build_body(self, request)
    }
}

fn truncate(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        s.to_string()
    } else {
        let mut end = max_bytes;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}... (truncated)", &s[..end])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openrouter_fails_if_no_key() {
        std::env::remove_var("OPENROUTER_API_KEY");
        assert!(OpenRouter::from_env().is_err());
    }

    #[test]
    fn openrouter_uses_env_vars() {
        std::env::set_var("OPENROUTER_API_KEY", "sk-or-testkey");
        std::env::set_var("OPENROUTER_MODEL", "openai/gpt-4o");
        std::env::set_var("OPENROUTER_URL", "https://custom.openrouter/v1");

        let p = OpenRouter::from_env().unwrap();
        assert_eq!(p.name(), "openrouter");
        assert_eq!(p.model(), "openai/gpt-4o");
        assert_eq!(
            p.chat_url(),
            "https://custom.openrouter/v1/chat/completions"
        );

        std::env::remove_var("OPENROUTER_API_KEY");
        std::env::remove_var("OPENROUTER_MODEL");
        std::env::remove_var("OPENROUTER_URL");
    }
}
