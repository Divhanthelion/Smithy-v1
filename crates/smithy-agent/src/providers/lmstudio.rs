//! LM Studio provider — the default and primary backend.
//!
//! Speaks LM Studio's OpenAI-compatible endpoint, targeting Qwen 3.6 27B.
//!
//! ## Why this is hand-rolled
//!
//! coda's rule, kept verbatim: *"Hand-rolled serde types, permissive (unknown
//! fields ignored) — do NOT adopt strict SDK types; LM Studio returns
//! non-standard fields."* A strict OpenAI SDK type rejects the response outright
//! the first time the server adds a field, and reasoning models are exactly
//! where servers add fields. Every wire struct below is `#[serde(default)]` on
//! every member and ignores what it does not recognise.
//!
//! The whole client is a few hundred lines. That is the point — it is small
//! enough to read in one sitting, and it cannot break in a way you can't see.

use std::time::Duration;

use async_trait::async_trait;
use futures_util::StreamExt;
use serde_json::{json, Value};
use smithy_tools::ToolCall;

use crate::provider::{Completion, CompletionRequest, Delta, Provider, ProviderError, Sampling};

/// Cold prefill of a large context genuinely takes minutes on local hardware,
/// so the request timeout is generous. Connect timeout stays short, because a
/// server that isn't listening should fail immediately rather than hang.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(900);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

pub struct LmStudio {
    http: reqwest::Client,
    base_url: String,
    model: String,
}

impl LmStudio {
    pub fn new(
        base_url: impl Into<String>,
        model: impl Into<String>,
    ) -> Result<Self, ProviderError> {
        let http = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|e| ProviderError::Other(format!("could not build HTTP client: {e}")))?;
        Ok(LmStudio {
            http,
            base_url: base_url.into().trim_end_matches('/').to_string(),
            model: model.into(),
        })
    }

    /// The conventional local defaults, overridable by `LMSTUDIO_URL` and
    /// `LMSTUDIO_MODEL` — the same names coda used, so an existing setup keeps
    /// working unchanged.
    pub fn from_env() -> Result<Self, ProviderError> {
        let base_url = std::env::var("LMSTUDIO_URL")
            .unwrap_or_else(|_| "http://localhost:1234/v1".to_string());
        let model = std::env::var("LMSTUDIO_MODEL").unwrap_or_else(|_| "qwen3.6-27b".to_string());
        LmStudio::new(base_url, model)
    }
    fn chat_url(&self) -> String {
        format!("{}/chat/completions", self.base_url)
    }

    fn models_url(&self) -> String {
        format!("{}/models", self.base_url)
    }

    /// LM Studio's *native* REST API, which carries metadata the
    /// OpenAI-compatible surface does not: whether a model is actually loaded,
    /// the context length it was loaded with, and whether it was trained for
    /// tool use. Derived from `base_url` by replacing the OpenAI path segment.
    fn native_models_url(&self) -> String {
        match self.base_url.rfind("/v1") {
            Some(i) => format!("{}/api/v1/models", &self.base_url[..i]),
            None => format!("{}/api/v1/models", self.base_url),
        }
    }

    /// Ask the native API about the configured model.
    ///
    /// Returns `Ok(None)` when the native API isn't available — older LM Studio
    /// builds only have the OpenAI surface, and this must degrade rather than
    /// hard-fail.
    pub async fn probe_model(&self) -> Result<Option<ModelInfo>, ProviderError> {
        let Ok(response) = self.http.get(self.native_models_url()).send().await else {
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
        let Some(models) = value["models"].as_array() else {
            return Ok(None);
        };

        let keys: Vec<String> = models
            .iter()
            .filter_map(|m| m["key"].as_str().map(str::to_string))
            .collect();
        let resolved = resolve_model(&self.model, &keys);

        let Some(entry) = models.iter().find(|m| m["key"].as_str() == Some(&resolved)) else {
            return Ok(Some(ModelInfo {
                key: resolved,
                found: false,
                ..Default::default()
            }));
        };

        // A model can be downloaded but not loaded. The OpenAI `/v1/models`
        // endpoint lists everything on disk, so coda's "is the id in the
        // response" check passed for models that were not actually resident —
        // and the failure surfaced later as a mysterious slow first request.
        let instances = entry["loaded_instances"].as_array();
        let loaded = instances.map(|i| !i.is_empty()).unwrap_or(false);

        // The context window this *instance* was loaded with, which is what the
        // KV cache is actually bounded by — not the model's theoretical maximum.
        let context_length = instances
            .and_then(|i| i.first())
            .and_then(|i| i["config"]["context_length"].as_i64());

        Ok(Some(ModelInfo {
            key: resolved,
            found: true,
            loaded,
            context_length,
            max_context_length: entry["max_context_length"].as_i64(),
            trained_for_tool_use: entry["capabilities"]["trained_for_tool_use"]
                .as_bool()
                .unwrap_or(true),
            format: entry["format"].as_str().unwrap_or_default().to_string(),
            quantization: entry["quantization"]["name"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
        }))
    }

    /// The request body. Always streamed — see [`LmStudio::complete`].
    fn build_body(&self, request: &CompletionRequest<'_>) -> Value {
        let stream = true;
        let s: &Sampling = request.sampling;
        let mut body = json!({
            "model": self.model,
            "messages": request.history.to_api(),
            "tools": request.tools,
            "stream": true,
            "temperature": s.temperature,
            "top_p": s.top_p,
            "top_k": s.top_k,
            "min_p": s.min_p,
            "repetition_penalty": s.repetition_penalty,
            "presence_penalty": s.presence_penalty,
            "max_tokens": s.max_tokens,
        });
        let _ = stream;
        // Without this the final chunk carries no usage block, and the context
        // budget has nothing to track.
        body["stream_options"] = json!({ "include_usage": true });
        body
    }

    async fn complete_streaming(
        &self,
        request: CompletionRequest<'_>,
        on_delta: Option<&(dyn Fn(Delta) + Send + Sync)>,
    ) -> Result<Completion, ProviderError> {
        let response = self
            .http
            .post(self.chat_url())
            .json(&self.build_body(&request))
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

            // SSE frames are newline-delimited, but a chunk can split one in
            // half — keep the trailing partial line in the buffer for next time.
            while let Some(newline) = buffer.find('\n') {
                let line = buffer[..newline].trim().to_string();
                buffer.drain(..=newline);
                if apply_sse_line(&line, &mut out, &mut partials, on_delta) {
                    // [DONE]
                    buffer.clear();
                    break;
                }
            }
        }

        out.tool_calls = partials
            .into_iter()
            .enumerate()
            .filter(|(_, p)| !p.name.is_empty())
            .map(|(i, p)| {
                ToolCall::new(
                    if p.id.is_empty() {
                        format!("call_{i}")
                    } else {
                        p.id
                    },
                    p.name,
                    p.arguments,
                )
            })
            .collect();
        Ok(out)
    }
}

/// Apply one `data:` line. Returns true when the stream is finished.
fn apply_sse_line(
    line: &str,
    out: &mut Completion,
    partials: &mut Vec<PartialCall>,
    on_delta: Option<&(dyn Fn(Delta) + Send + Sync)>,
) -> bool {
    let Some(data) = line.strip_prefix("data:") else {
        return false;
    };
    let data = data.trim();
    if data == "[DONE]" {
        return true;
    }
    let Ok(v) = serde_json::from_str::<Value>(data) else {
        return false; // a frame we can't read is not worth failing the turn over
    };

    if v.get("usage").map(|u| !u.is_null()).unwrap_or(false) {
        out.prompt_tokens = v["usage"]["prompt_tokens"]
            .as_i64()
            .unwrap_or(out.prompt_tokens);
        out.completion_tokens = v["usage"]["completion_tokens"]
            .as_i64()
            .unwrap_or(out.completion_tokens);
    }

    let Some(choice) = v["choices"].get(0) else {
        return false;
    };
    let delta = &choice["delta"];

    // Reasoning arrives in its own field, cleanly separated from content — not
    // as inline `<think>` tags. Shown live, never appended to history.
    for key in ["reasoning_content", "reasoning"] {
        if let Some(t) = delta[key].as_str() {
            if !t.is_empty() {
                out.reasoning.push_str(t);
                if let Some(f) = on_delta {
                    f(Delta::Reasoning(t.to_string()));
                }
            }
        }
    }
    if let Some(t) = delta["content"].as_str() {
        if !t.is_empty() {
            out.content.push_str(t);
            if let Some(f) = on_delta {
                f(Delta::Content(t.to_string()));
            }
        }
    }

    // Tool calls stream in fragments keyed by index; the name arrives once and
    // the arguments accumulate across many frames.
    if let Some(arr) = delta["tool_calls"].as_array() {
        for tc in arr {
            let idx = tc["index"].as_u64().unwrap_or(0) as usize;
            while partials.len() <= idx {
                partials.push(PartialCall::default());
            }
            let p = &mut partials[idx];
            if let Some(id) = tc["id"].as_str() {
                if !id.is_empty() {
                    p.id = id.to_string();
                }
            }
            if let Some(name) = tc["function"]["name"].as_str() {
                if p.name.is_empty() {
                    p.name = name.to_string();
                }
            }
            if let Some(a) = tc["function"]["arguments"].as_str() {
                p.arguments.push_str(a);
            }
        }
    }
    if let Some(fr) = choice["finish_reason"].as_str() {
        out.finish_reason = fr.to_string();
    }
    false
}

#[async_trait]
impl Provider for LmStudio {
    fn name(&self) -> &str {
        "lmstudio"
    }

    fn model(&self) -> &str {
        &self.model
    }

    /// Fail early and legibly rather than as a mysterious failed turn.
    ///
    /// Prefers the native API, which can distinguish "downloaded" from "loaded"
    /// — a difference the OpenAI surface cannot express, and one that otherwise
    /// shows up as an inexplicably slow first request.
    async fn preflight(&self) -> Result<(), ProviderError> {
        if let Some(info) = self.probe_model().await? {
            if !info.found {
                return Err(ProviderError::ModelNotLoaded {
                    model: self.model.clone(),
                    endpoint: self.native_models_url(),
                });
            }
            if !info.loaded {
                return Err(ProviderError::Other(format!(
                    "model `{}` is downloaded but not loaded. Load it in LM Studio (or POST to \
                     /api/v1/models/load), then try again.",
                    info.key
                )));
            }
            if !info.trained_for_tool_use {
                return Err(ProviderError::Other(format!(
                    "model `{}` reports that it was not trained for tool use. Smithy drives \
                     everything through tool calls, so it will not work well. Load a tool-capable \
                     model instead.",
                    info.key
                )));
            }
            return Ok(());
        }

        // Older LM Studio: only the OpenAI surface exists. Fall back to the
        // weaker "is the id present" check.
        let response = self.http.get(self.models_url()).send().await.map_err(|e| {
            ProviderError::Unreachable {
                endpoint: self.models_url(),
                source: Box::new(e),
            }
        })?;

        if !response.status().is_success() {
            return Err(ProviderError::Http {
                status: response.status().as_u16(),
                body: format!("GET {}", self.models_url()),
            });
        }

        let text = response.text().await.unwrap_or_default();
        let available = parse_model_ids(&text);
        let resolved = resolve_model(&self.model, &available);
        if !available.contains(&resolved) {
            return Err(ProviderError::ModelNotLoaded {
                model: self.model.clone(),
                endpoint: self.models_url(),
            });
        }
        Ok(())
    }

    async fn complete(
        &self,
        request: CompletionRequest<'_>,
        on_delta: Option<&(dyn Fn(Delta) + Send + Sync)>,
    ) -> Result<Completion, ProviderError> {
        // Streaming only. The blocking path and its wire types were removed:
        // `stream` was hardcoded true with no way to set it, so nothing could
        // reach them — while nine tests exercised them and passed, which is
        // exactly the shape HANDOFF §8 warns about.
        self.complete_streaming(request, on_delta).await
    }
}

/// What LM Studio's native API knows about a model.
///
/// Everything here is read from the server rather than assumed. `context_length`
/// in particular replaces a guess: coda hardcoded a 110k ceiling reasoned to sit
/// "just under the 131k KV ceiling", and the real number is reported directly by
/// the loaded instance.
#[derive(Debug, Default, Clone)]
pub struct ModelInfo {
    pub key: String,
    /// Whether a model with this key exists on the server at all.
    pub found: bool,
    /// Whether it is resident in memory. Downloaded is not loaded.
    pub loaded: bool,
    /// The context window this instance was loaded with — the real KV bound.
    pub context_length: Option<i64>,
    /// The largest window the model supports, which may be far larger than the
    /// window it was actually loaded with.
    pub max_context_length: Option<i64>,
    pub trained_for_tool_use: bool,
    pub format: String,
    pub quantization: String,
}

impl ModelInfo {
    /// Budget ceilings derived from the loaded context window.
    ///
    /// The hard stop leaves 15% headroom below the KV bound, because the prompt
    /// is not the only thing in the window — the response has to fit too, and
    /// running the prompt right up to the edge leaves no room to answer.
    /// Falls back to conservative defaults when the server didn't say.
    pub fn suggested_limits(&self) -> crate::limits::Limits {
        let defaults = crate::limits::Limits::default();
        let Some(ctx) = self.context_length else {
            return defaults;
        };
        crate::limits::Limits {
            context_hard: (ctx as f64 * 0.85) as i64,
            context_warn: (ctx as f64 * 0.25) as i64,
            ..defaults
        }
    }
}

#[derive(Default)]
struct PartialCall {
    id: String,
    name: String,
    arguments: String,
}

/// Extract the `id` of every entry in a `/v1/models` response.
fn parse_model_ids(body: &str) -> Vec<String> {
    serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| v["data"].as_array().cloned())
        .map(|arr| {
            arr.iter()
                .filter_map(|m| m["id"].as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default()
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        s.chars().take(n).collect::<String>() + "…"
    }
}

/// Pick the model id to use from what the server actually has loaded.
///
/// Local model ids drift constantly — quantization suffixes (`@8bit`), backend
/// suffixes (`-mlx`), publisher prefixes. A configured id that no longer matches
/// exactly would fail preflight even though the model is right there, so:
/// exact match wins, then a unique case-insensitive substring match either way,
/// and otherwise the configured id is kept so the error names what was asked for.
fn resolve_model(configured: &str, available: &[String]) -> String {
    if available.iter().any(|m| m == configured) {
        return configured.to_string();
    }
    let needle = configured.to_lowercase();
    let matches: Vec<&String> = available
        .iter()
        .filter(|m| {
            let m = m.to_lowercase();
            m.contains(&needle) || needle.contains(&m)
        })
        .collect();
    if matches.len() == 1 {
        return matches[0].clone();
    }
    configured.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- streaming reassembly ---

    fn drain(lines: &[&str]) -> (Completion, Vec<PartialCall>) {
        let mut out = Completion::default();
        let mut partials = Vec::new();
        for line in lines {
            apply_sse_line(line, &mut out, &mut partials, None);
        }
        (out, partials)
    }

    #[test]
    fn streamed_content_is_concatenated() {
        let (out, _) = drain(&[
            r#"data: {"choices":[{"delta":{"content":"Hello"}}]}"#,
            r#"data: {"choices":[{"delta":{"content":", world"}}]}"#,
            r#"data: {"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
        ]);
        assert_eq!(out.content, "Hello, world");
        assert_eq!(out.finish_reason, "stop");
    }

    /// Arguments arrive one fragment at a time and must reassemble into valid
    /// JSON, keyed by index so parallel calls do not interleave.
    #[test]
    fn streamed_tool_call_arguments_reassemble() {
        let (_, partials) = drain(&[
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"read","arguments":"{\"pa"}}]}}]}"#,
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"th\":\"a.rs\"}"}}]}}]}"#,
        ]);
        assert_eq!(partials[0].name, "read");
        assert_eq!(partials[0].arguments, r#"{"path":"a.rs"}"#);
        assert_eq!(partials[0].id, "c1");
    }

    #[test]
    fn parallel_streamed_tool_calls_stay_separate() {
        let (_, partials) = drain(&[
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"a","function":{"name":"ls","arguments":"{}"}}]}}]}"#,
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":1,"id":"b","function":{"name":"read","arguments":"{}"}}]}}]}"#,
        ]);
        assert_eq!(partials.len(), 2);
        assert_eq!(partials[0].name, "ls");
        assert_eq!(partials[1].name, "read");
    }

    #[test]
    fn usage_is_captured_from_the_final_frame() {
        let (out, _) = drain(&[
            r#"data: {"choices":[{"delta":{"content":"x"}}]}"#,
            r#"data: {"choices":[],"usage":{"prompt_tokens":1234,"completion_tokens":56}}"#,
        ]);
        assert_eq!(out.prompt_tokens, 1234);
        assert_eq!(out.completion_tokens, 56);
    }

    #[test]
    fn reasoning_deltas_accumulate_separately_from_content() {
        let (out, _) = drain(&[
            r#"data: {"choices":[{"delta":{"reasoning_content":"let me think"}}]}"#,
            r#"data: {"choices":[{"delta":{"content":"the answer"}}]}"#,
        ]);
        assert_eq!(out.reasoning, "let me think");
        assert_eq!(out.content, "the answer");
    }

    #[test]
    fn done_terminates_and_junk_frames_are_skipped() {
        let mut out = Completion::default();
        let mut partials = Vec::new();
        assert!(!apply_sse_line(
            "data: {not json}",
            &mut out,
            &mut partials,
            None
        ));
        assert!(!apply_sse_line("", &mut out, &mut partials, None));
        assert!(!apply_sse_line(
            ": a comment",
            &mut out,
            &mut partials,
            None
        ));
        assert!(apply_sse_line(
            "data: [DONE]",
            &mut out,
            &mut partials,
            None
        ));
    }

    #[test]
    fn deltas_reach_the_callback() {
        use std::sync::Mutex;
        let seen = Mutex::new(Vec::new());
        let sink = |d: Delta| {
            let s = match d {
                Delta::Reasoning(t) => format!("R:{t}"),
                Delta::Content(t) => format!("C:{t}"),
            };
            seen.lock().unwrap().push(s);
        };
        let mut out = Completion::default();
        let mut partials = Vec::new();
        apply_sse_line(
            r#"data: {"choices":[{"delta":{"reasoning_content":"hm","content":"hi"}}]}"#,
            &mut out,
            &mut partials,
            Some(&sink),
        );
        let seen = seen.lock().unwrap();
        assert_eq!(*seen, vec!["R:hm".to_string(), "C:hi".to_string()]);
    }

    #[test]
    fn env_defaults_match_codas_names() {
        let p = LmStudio::new("http://localhost:1234/v1", "qwen3.6-27b").unwrap();
        assert_eq!(p.chat_url(), "http://localhost:1234/v1/chat/completions");
        assert_eq!(p.models_url(), "http://localhost:1234/v1/models");
    }

    #[test]
    fn a_trailing_slash_in_the_base_url_is_tolerated() {
        let p = LmStudio::new("http://localhost:1234/v1/", "m").unwrap();
        assert_eq!(p.chat_url(), "http://localhost:1234/v1/chat/completions");
    }
}

#[cfg(test)]
mod resolve_tests {
    use super::*;

    /// The real `/v1/models` payload from the machine this targets.
    fn available() -> Vec<String> {
        [
            "qwen3.6-27b",
            "qwen3-embedding-8b-mxfp8",
            "qwen3.5-0.8b-mlx",
            "qwen3.5-4b-mlx",
            "gemma-4-31b-it-mlx",
            "qwen3.5-35b-a3b",
            "qwen3-tts-12hz-1.7b-base",
            "nvidia/nemotron-3-nano",
            "qwen3-asr-1.7b@8bit",
            "text-embedding-nomic-embed-text-v2-moe",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect()
    }

    #[test]
    fn parses_ids_from_a_models_response() {
        let body = r#"{"data":[{"id":"a","object":"model"},{"id":"b"}],"object":"list"}"#;
        assert_eq!(
            parse_model_ids(body),
            vec!["a".to_string(), "b".to_string()]
        );
    }

    #[test]
    fn a_junk_models_response_yields_nothing_rather_than_panicking() {
        assert!(parse_model_ids("not json").is_empty());
        assert!(parse_model_ids("{}").is_empty());
    }

    #[test]
    fn an_exact_id_is_used_as_is() {
        assert_eq!(resolve_model("qwen3.6-27b", &available()), "qwen3.6-27b");
    }

    /// A quantization suffix should not require reconfiguring.
    #[test]
    fn a_unique_substring_match_resolves() {
        assert_eq!(
            resolve_model("qwen3-asr-1.7b", &available()),
            "qwen3-asr-1.7b@8bit"
        );
    }

    /// Nor should the reverse: a configured id carrying a suffix the server
    /// doesn't use.
    #[test]
    fn a_configured_id_containing_the_server_id_resolves() {
        assert_eq!(
            resolve_model("qwen3.6-27b@8bit", &available()),
            "qwen3.6-27b"
        );
    }

    /// An ambiguous prefix must NOT silently pick one — `qwen3.5` matches four
    /// models, and guessing would quietly run the wrong one.
    #[test]
    fn an_ambiguous_match_is_left_alone_to_fail_loudly() {
        assert_eq!(resolve_model("qwen3.5", &available()), "qwen3.5");
    }

    #[test]
    fn an_unknown_model_is_preserved_so_the_error_names_it() {
        assert_eq!(resolve_model("llama-99b", &available()), "llama-99b");
    }
}

#[cfg(test)]
mod native_api_tests {
    use super::*;

    #[test]
    fn derives_the_native_url_from_the_openai_base() {
        let p = LmStudio::new("http://localhost:1234/v1", "m").unwrap();
        assert_eq!(p.native_models_url(), "http://localhost:1234/api/v1/models");
    }

    #[test]
    fn derives_the_native_url_when_the_base_has_no_v1() {
        let p = LmStudio::new("http://box:1234", "m").unwrap();
        assert_eq!(p.native_models_url(), "http://box:1234/api/v1/models");
    }

    /// Limits derived from the loaded window, not guessed.
    /// The live server reports `context_length: 131072` for this instance.
    #[test]
    fn limits_are_derived_from_the_loaded_context_length() {
        let info = ModelInfo {
            context_length: Some(131_072),
            ..Default::default()
        };
        let limits = info.suggested_limits();
        assert_eq!(limits.context_hard, 111_411);
        assert_eq!(limits.context_warn, 32_768);
        assert!(
            limits.context_hard < 131_072,
            "the hard stop must leave room for the response"
        );
    }

    #[test]
    fn limits_fall_back_to_defaults_when_the_server_is_silent() {
        let info = ModelInfo::default();
        let defaults = crate::limits::Limits::default();
        assert_eq!(info.suggested_limits().context_hard, defaults.context_hard);
    }

    /// A model loaded with a smaller window than it supports must be budgeted
    /// against the window it actually has.
    #[test]
    fn a_reduced_load_window_lowers_the_ceiling() {
        let info = ModelInfo {
            context_length: Some(8192),
            max_context_length: Some(262_144),
            ..Default::default()
        };
        assert!(info.suggested_limits().context_hard < 8192);
    }
}
