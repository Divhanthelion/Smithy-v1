//! DeepSeek provider implementation.
//!
//! Connects to DeepSeek's OpenAI-compatible API (`https://api.deepseek.com`),
//! authenticated via `DEEPSEEK_API_KEY` or the key held in the OS credential
//! store.
//!
//! ## Why this is a third provider rather than OpenRouter with a different URL
//!
//! It is very nearly that, and the SSE layer *is* shared — [`crate::providers::sse`]
//! already parses both, including the `reasoning_content` channel DeepSeek emits.
//! Three things stopped it being a one-line reuse:
//!
//! - **Preflight.** OpenRouter's runs against `/auth/key`, which DeepSeek does
//!   not have. Pointed at DeepSeek it would 404, and that path only treats
//!   401/403 as failure — so a wrong key would sail through preflight and fail
//!   later as a broken turn, which is precisely what preflight exists to stop.
//! - **The model probe.** OpenRouter reports `context_length` per model;
//!   DeepSeek's `/models` returns ids and nothing else. Reusing that parse would
//!   silently drop to the conservative default ceiling and give up the 1M window.
//! - **The name.** `Provider::name` is displayed, and calling DeepSeek
//!   "openrouter" on screen is the kind of small lie that costs an hour later.
//!
//! ## Context windows come from a table, not the wire
//!
//! `/models` gives no metadata at all, so [`context_for`] is a static map. That
//! is a real limitation: a model DeepSeek adds after this was written gets the
//! conservative default until the table is updated. It degrades in the safe
//! direction — an under-estimated window means the budget stops a turn early
//! rather than letting the endpoint reject an over-long prompt.

use std::time::Duration;

use async_trait::async_trait;
use futures_util::StreamExt;
use serde_json::{json, Value};

use crate::provider::{Completion, CompletionRequest, Delta, Provider, ProviderError, Sampling};
use crate::providers::lmstudio::ModelInfo;
use crate::providers::sse::{apply_sse_line, build_tool_calls, PartialCall};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(900);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// The default endpoint. DeepSeek accepts `/v1` as well and treats it as
/// meaningless — their docs say outright that the `v1` has nothing to do with
/// the model version — so the shorter form is used and either will work if
/// someone types it.
pub const DEFAULT_URL: &str = "https://api.deepseek.com";

/// The default model: the cheaper of the two, and the one their Responses API
/// supports.
pub const DEFAULT_MODEL: &str = "deepseek-v4-flash";

/// What is known about DeepSeek's models, as of August 2026.
///
/// `(id, context_length, prompt $/Mtok, completion $/Mtok)`.
///
/// **A snapshot, and it will drift.** `/models` carries no metadata, so there is
/// nothing to read this from at runtime. Prices are the off-peak list rates;
/// DeepSeek has announced peak-hour rates at double these. Treat the numbers as
/// a guide for choosing a model, never as a billing estimate — the authority is
/// their pricing page.
pub const KNOWN_MODELS: &[(&str, i64, f64, f64)] = &[
    ("deepseek-v4-flash", 1_000_000, 0.14, 0.28),
    ("deepseek-v4-pro", 1_000_000, 0.435, 0.87),
];

/// The context window for a model id, when it is one we know about.
pub fn context_for(model: &str) -> Option<i64> {
    KNOWN_MODELS
        .iter()
        .find(|(id, ..)| *id == model)
        .map(|(_, ctx, ..)| *ctx)
}

/// List prices for a model id, in dollars per million tokens.
pub fn pricing_for(model: &str) -> Option<(f64, f64)> {
    KNOWN_MODELS
        .iter()
        .find(|(id, ..)| *id == model)
        .map(|(_, _, prompt, completion)| (*prompt, *completion))
}

pub struct DeepSeek {
    http: reqwest::Client,
    base_url: String,
    model: String,
    api_key: String,
}

impl DeepSeek {
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

        Ok(DeepSeek {
            http,
            base_url: base_url.into().trim_end_matches('/').to_string(),
            model: model.into(),
            api_key: api_key.into(),
        })
    }

    /// Read configuration from the environment, for callers without a settings
    /// file. See [`crate::config`] for the path the app actually takes.
    pub fn from_env() -> Result<Self, ProviderError> {
        let api_key = std::env::var("DEEPSEEK_API_KEY").map_err(|_| {
            ProviderError::Other(
                "DEEPSEEK_API_KEY is not set. Add a key under Settings → Agent, or set \
                 DEEPSEEK_API_KEY=<key>."
                    .to_string(),
            )
        })?;
        if api_key.trim().is_empty() {
            return Err(ProviderError::Other(
                "DEEPSEEK_API_KEY is empty.".to_string(),
            ));
        }
        let base_url = std::env::var("DEEPSEEK_URL").unwrap_or_else(|_| DEFAULT_URL.to_string());
        let model = std::env::var("DEEPSEEK_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.to_string());
        DeepSeek::new(base_url, model, api_key)
    }

    fn chat_url(&self) -> String {
        format!("{}/chat/completions", self.base_url)
    }

    fn models_url(&self) -> String {
        format!("{}/models", self.base_url)
    }

    fn build_body(&self, request: &CompletionRequest<'_>) -> Value {
        let s: &Sampling = request.sampling;
        let mut body = json!({
            "model": self.model,
            "messages": request.history.to_api(),
            "tools": request.tools,
            "stream": true,
            "temperature": s.temperature,
            "top_p": s.top_p,
            "max_tokens": s.max_tokens,
        });
        // Without this the final chunk carries no usage block, and the context
        // budget has nothing to track.
        body["stream_options"] = json!({ "include_usage": true });
        body
    }
}

#[async_trait]
impl Provider for DeepSeek {
    fn name(&self) -> &str {
        "deepseek"
    }

    fn model(&self) -> &str {
        &self.model
    }

    /// Confirm the key works, using `/models`.
    ///
    /// DeepSeek has no `/auth/key`, so the cheapest authenticated call stands in
    /// for one. A 401 here is a wrong key and is worth saying plainly; anything
    /// else is let through, because a listing endpoint being unhappy is not
    /// grounds for refusing to start a session.
    async fn preflight(&self) -> Result<(), ProviderError> {
        if self.api_key.trim().is_empty() {
            return Err(ProviderError::Other(
                "DeepSeek needs an API key. Add one under Settings → Agent.".to_string(),
            ));
        }

        let response = self
            .http
            .get(self.models_url())
            .bearer_auth(&self.api_key)
            .send()
            .await
            .map_err(|e| ProviderError::Unreachable {
                endpoint: self.models_url(),
                source: Box::new(e),
            })?;

        let status = response.status();
        if status.as_u16() == 401 || status.as_u16() == 403 {
            return Err(ProviderError::Http {
                status: status.as_u16(),
                body: "DeepSeek rejected the API key. Check it under Settings → Agent.".to_string(),
            });
        }
        Ok(())
    }

    /// Report what is known about the configured model.
    ///
    /// The context window comes from [`KNOWN_MODELS`] rather than the wire — see
    /// the module docs. `found` reflects whether `/models` actually lists the id,
    /// which does catch the ordinary typo.
    async fn probe_model(&self) -> Result<Option<ModelInfo>, ProviderError> {
        let listed = match self
            .http
            .get(self.models_url())
            .bearer_auth(&self.api_key)
            .send()
            .await
        {
            Ok(response) if response.status().is_success() => response
                .json::<Value>()
                .await
                .ok()
                .and_then(|body| {
                    body["data"].as_array().map(|models| {
                        models
                            .iter()
                            .filter_map(|m| m["id"].as_str().map(str::to_string))
                            .collect::<Vec<_>>()
                    })
                })
                .unwrap_or_default(),
            // An unreachable or unhappy listing endpoint must not stop a session
            // that would otherwise work.
            _ => Vec::new(),
        };

        let found = listed.is_empty() || listed.iter().any(|id| id == &self.model);
        let context = context_for(&self.model);

        Ok(Some(ModelInfo {
            key: self.model.clone(),
            found,
            loaded: true,
            context_length: context,
            max_context_length: context,
            trained_for_tool_use: true,
            format: "cloud".to_string(),
            quantization: "api".to_string(),
        }))
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

        let mut out = Completion::default();
        let mut partials: Vec<PartialCall> = Vec::new();
        let mut buffer = String::new();
        let mut stream = response.bytes_stream();

        while let Some(chunk) = stream.next().await {
            let chunk =
                chunk.map_err(|e| ProviderError::BadResponse(format!("stream read error: {e}")))?;
            buffer.push_str(&String::from_utf8_lossy(&chunk));

            while let Some(newline) = buffer.find('\n') {
                let line = buffer[..newline].trim().to_string();
                buffer.drain(..=newline);
                if apply_sse_line(&line, &mut out, &mut partials, on_delta) {
                    buffer.clear();
                    break;
                }
            }
        }

        out.tool_calls = build_tool_calls(partials);
        Ok(out)
    }

    fn build_body(&self, request: &CompletionRequest<'_>) -> Value {
        DeepSeek::build_body(self, request)
    }
}

fn truncate(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let mut end = max_bytes;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}... (truncated)", &s[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_are_built_off_the_base() {
        let p = DeepSeek::new(DEFAULT_URL, DEFAULT_MODEL, "k").unwrap();
        assert_eq!(p.chat_url(), "https://api.deepseek.com/chat/completions");
        assert_eq!(p.models_url(), "https://api.deepseek.com/models");
    }

    /// A trailing slash is the commonest thing to paste, and doubling it would
    /// produce a 404 that names nothing.
    #[test]
    fn a_trailing_slash_does_not_double_up() {
        let p = DeepSeek::new("https://api.deepseek.com/", DEFAULT_MODEL, "k").unwrap();
        assert_eq!(p.chat_url(), "https://api.deepseek.com/chat/completions");
    }

    /// DeepSeek treats `/v1` as meaningless but accepts it, so someone who
    /// pastes it must still get a working URL.
    #[test]
    fn a_v1_suffix_still_produces_a_valid_route() {
        let p = DeepSeek::new("https://api.deepseek.com/v1", DEFAULT_MODEL, "k").unwrap();
        assert_eq!(p.chat_url(), "https://api.deepseek.com/v1/chat/completions");
    }

    #[test]
    fn the_provider_names_itself_deepseek() {
        let p = DeepSeek::new(DEFAULT_URL, DEFAULT_MODEL, "k").unwrap();
        assert_eq!(p.name(), "deepseek");
        assert_eq!(p.model(), DEFAULT_MODEL);
    }

    #[test]
    fn known_models_carry_a_context_window_and_a_price() {
        assert_eq!(context_for("deepseek-v4-flash"), Some(1_000_000));
        assert_eq!(context_for("deepseek-v4-pro"), Some(1_000_000));
        assert_eq!(pricing_for("deepseek-v4-flash"), Some((0.14, 0.28)));
    }

    /// A model added after this was written must degrade to "unknown", not to a
    /// wrong number borrowed from a different model.
    #[test]
    fn an_unknown_model_reports_no_context_rather_than_guessing() {
        assert_eq!(context_for("deepseek-v9-unreleased"), None);
        assert_eq!(pricing_for("deepseek-v9-unreleased"), None);
    }

    #[tokio::test]
    async fn an_empty_key_fails_preflight_without_a_request() {
        let p = DeepSeek::new(DEFAULT_URL, DEFAULT_MODEL, "   ").unwrap();
        let err = p.preflight().await.unwrap_err();
        assert!(err.to_string().contains("API key"), "{err}");
    }

    #[test]
    fn the_request_body_asks_for_usage_so_the_budget_can_track_it() {
        let p = DeepSeek::new(DEFAULT_URL, DEFAULT_MODEL, "k").unwrap();
        let history = crate::message::History::new("system");
        let tools = json!([]);
        let sampling = Sampling::default();
        let body = p.build_body(&CompletionRequest {
            history: &history,
            tools: &tools,
            sampling: &sampling,
            timeout: None,
        });
        assert_eq!(body["stream"], json!(true));
        assert_eq!(body["stream_options"]["include_usage"], json!(true));
        assert_eq!(body["model"], json!(DEFAULT_MODEL));
    }
}
