//! The provider interface the loop runs against.
//!
//! Narrower than forge's `AiProvider` on purpose: the loop needs exactly one
//! operation — turn a history plus a tool schema into a [`Completion`] — and
//! keeping the surface to that means a provider implementation is a couple of
//! hundred lines and the loop cannot accidentally depend on backend specifics.
//!
//! [`Completion`] carries `prompt_tokens` because the whole context-budget
//! design depends on it: the endpoint reports its own token accounting on every
//! response, so there is no local tokenizer to install, keep in sync with the
//! model, or be wrong.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use smithy_tools::ToolCall;
use thiserror::Error;

use crate::message::History;

#[derive(Debug, Error)]
pub enum ProviderError {
    #[error("cannot reach {endpoint}: {source}. Is the server running?")]
    Unreachable {
        endpoint: String,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    #[error("model `{model}` is not available at {endpoint}")]
    ModelNotLoaded { model: String, endpoint: String },

    #[error("HTTP {status}: {body}")]
    Http { status: u16, body: String },

    #[error("could not parse the response: {0}")]
    BadResponse(String),

    #[error("{0}")]
    Other(String),
}

/// Sampling knobs.
///
/// The defaults are coda's, chosen for a local Qwen-class model doing tool
/// calling. `max_tokens` is deliberately generous: a truncated reasoning block
/// never emits its closing tag, which breaks downstream parsing, so running out
/// of output budget is worse than spending it.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Sampling {
    pub temperature: f64,
    pub top_p: f64,
    pub top_k: i64,
    pub min_p: f64,
    pub repetition_penalty: f64,
    pub presence_penalty: f64,
    pub max_tokens: i64,
}

impl Default for Sampling {
    fn default() -> Self {
        Sampling {
            temperature: 0.6,
            top_p: 0.95,
            top_k: 20,
            min_p: 0.03,
            repetition_penalty: 1.0,
            presence_penalty: 0.0,
            max_tokens: 16384,
        }
    }
}

/// One request for a completion.
pub struct CompletionRequest<'a> {
    pub history: &'a History,
    /// The tool schema array, built once per session and reused verbatim.
    pub tools: &'a Value,
    pub sampling: &'a Sampling,
    /// Remaining turn budget. Providers apply `min(this, their configured
    /// timeout)` on the HTTP request so a stuck completion cannot outlive the
    /// turn clock. `None` keeps the client-level timeout (tests, probes).
    pub timeout: Option<std::time::Duration>,
}

impl CompletionRequest<'_> {
    /// Bound this request by both the turn clock and the backend's own cap.
    pub fn http_timeout(&self, configured: std::time::Duration) -> std::time::Duration {
        match self.timeout {
            Some(remaining) => remaining.min(configured),
            None => configured,
        }
    }
}

/// A streamed fragment.
pub enum Delta {
    /// The model's reasoning channel. Displayed live, but **never fed back into
    /// history** — it is not part of the cached prefix the endpoint replays.
    Reasoning(String),
    Content(String),
}

/// A normalized completion.
#[derive(Debug, Default, Clone)]
pub struct Completion {
    pub content: String,
    /// Reasoning, surfaced separately by endpoints that support it.
    pub reasoning: String,
    pub tool_calls: Vec<ToolCall>,
    pub finish_reason: String,
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    /// Tokens spent in the reasoning channel, when the endpoint reports them.
    ///
    /// Read from `usage.completion_tokens_details.reasoning_tokens`, which LM
    /// Studio returns. coda estimated this with an invented `chars/4 + chars/18`
    /// formula and its post-mortem calls that out — *"I had the real number and
    /// reported a guess."* It is right there in the response.
    pub reasoning_tokens: i64,
    /// Prompt tokens served from the provider's prefix cache on this request.
    ///
    /// Parsed tolerantly — see [`crate::providers::sse::cached_tokens_from_usage`].
    /// Zero means a reported miss; absence of any cache field leaves this at
    /// the default zero and is indistinguishable from a miss until a later
    /// frame reports a positive count.
    pub cached_tokens: i64,
}

impl Completion {
    /// Whether the model ran out of output budget mid-sentence.
    pub fn was_truncated(&self) -> bool {
        self.finish_reason == "length"
    }

    /// Whether this completion carries nothing usable.
    ///
    /// The distinction matters: an empty answer with no tool calls is the
    /// signature of the high-context reasoning loop, not of a finished turn.
    pub fn is_empty(&self) -> bool {
        self.content.trim().is_empty() && self.tool_calls.is_empty()
    }
}

#[async_trait]
pub trait Provider: Send + Sync {
    fn name(&self) -> &str;
    fn model(&self) -> &str;

    /// Confirm the endpoint is reachable and the model is loaded, with an error
    /// the user can act on. Called once at session start so a misconfiguration
    /// surfaces immediately rather than as a failed turn.
    async fn preflight(&self) -> Result<(), ProviderError> {
        Ok(())
    }

    /// Probe the model endpoint for details (context length, loaded status, formatting).
    async fn probe_model(&self) -> Result<Option<crate::providers::ModelInfo>, ProviderError> {
        Ok(None)
    }

    /// Whether tool-calling assistant messages must send `reasoning_content`
    /// back on the next request. DeepSeek V4 400s without it. Default off —
    /// the trace is stored on the Message either way; this only controls the
    /// POST body.
    fn round_trip_reasoning(&self) -> bool {
        false
    }

    /// After a write lands, rewrite earlier `read`/`edit`/`write` payloads for
    /// that path. A miss of the prefix cache is cheaper than attending to a
    /// stale file. Local KV caches (LM Studio) leave this off — Compact is
    /// their rewrite.
    fn stub_superseded_snapshots(&self) -> bool {
        false
    }

    /// Get one completion. `on_delta`, when present, is called as fragments
    /// arrive; the assembled completion is returned either way.
    async fn complete(
        &self,
        request: CompletionRequest<'_>,
        on_delta: Option<&(dyn Fn(Delta) + Send + Sync)>,
    ) -> Result<Completion, ProviderError>;

    /// JSON body this provider would POST for `request`.
    ///
    /// Inspection reads this, not a reconstruction, so an adapter that munges
    /// the messages is visible. The default is the OpenAI `messages` + `tools`
    /// pair; real providers add sampling and stream flags.
    fn build_body(&self, request: &CompletionRequest<'_>) -> Value {
        serde_json::json!({
            "messages": request.history.to_api_with_reasoning(self.round_trip_reasoning()),
            "tools": request.tools,
        })
    }
}

/// A provider that replays a script, and the helpers for writing one.
///
/// Gated on `cfg(test)` **or** the `testing` feature. Behind `cfg(test)` alone it
/// was reachable only from unit tests inside this crate, which meant every
/// multi-turn scenario had to live next to the code it exercised or not exist —
/// and `tests/` could not reach it at all. The feature is not a default, so a
/// release build still cannot construct a fake provider.
#[cfg(any(test, feature = "testing"))]
pub mod test_support {
    use super::*;
    use std::sync::Mutex;

    /// A provider that replays a fixed script of completions.
    pub struct ScriptedProvider {
        script: Mutex<std::collections::VecDeque<Completion>>,
        pub calls: Mutex<usize>,
        stub_superseded: bool,
    }

    impl ScriptedProvider {
        pub fn new(script: Vec<Completion>) -> Self {
            Self {
                script: Mutex::new(script.into()),
                calls: Mutex::new(0),
                stub_superseded: false,
            }
        }

        /// Opt into rewriting stale file snapshots after a write lands.
        pub fn stubbing_superseded(script: Vec<Completion>) -> Self {
            Self {
                script: Mutex::new(script.into()),
                calls: Mutex::new(0),
                stub_superseded: true,
            }
        }

        pub fn call_count(&self) -> usize {
            *self.calls.lock().unwrap()
        }
    }

    #[async_trait]
    impl Provider for ScriptedProvider {
        fn name(&self) -> &str {
            "scripted"
        }
        fn model(&self) -> &str {
            "test-model"
        }
        fn stub_superseded_snapshots(&self) -> bool {
            self.stub_superseded
        }
        async fn complete(
            &self,
            _request: CompletionRequest<'_>,
            _on_delta: Option<&(dyn Fn(Delta) + Send + Sync)>,
        ) -> Result<Completion, ProviderError> {
            *self.calls.lock().unwrap() += 1;
            self.script
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| ProviderError::Other("script exhausted".into()))
        }
    }

    /// A provider whose `complete` never returns. The turn clock has to cut it
    /// off — a client-level HTTP timeout of an hour would not.
    pub struct HangingProvider {
        pub calls: Mutex<usize>,
    }

    impl Default for HangingProvider {
        fn default() -> Self {
            Self::new()
        }
    }

    impl HangingProvider {
        pub fn new() -> Self {
            Self {
                calls: Mutex::new(0),
            }
        }

        pub fn call_count(&self) -> usize {
            *self.calls.lock().unwrap()
        }
    }

    #[async_trait]
    impl Provider for HangingProvider {
        fn name(&self) -> &str {
            "hanging"
        }
        fn model(&self) -> &str {
            "hang"
        }
        async fn complete(
            &self,
            _request: CompletionRequest<'_>,
            _on_delta: Option<&(dyn Fn(Delta) + Send + Sync)>,
        ) -> Result<Completion, ProviderError> {
            *self.calls.lock().unwrap() += 1;
            std::future::pending().await
        }
    }

    pub fn answer(text: &str) -> Completion {
        Completion {
            content: text.into(),
            finish_reason: "stop".into(),
            prompt_tokens: 100,
            ..Default::default()
        }
    }

    pub fn tool_call(id: &str, name: &str, args: &str) -> Completion {
        Completion {
            tool_calls: vec![ToolCall::new(id, name, args)],
            finish_reason: "tool_calls".into(),
            prompt_tokens: 100,
            ..Default::default()
        }
    }

    /// Several tool calls in one completion, which is what the endpoint sends
    /// when the model decides to fan out. Their results must come back matched by
    /// id, not by name or by order.
    pub fn tool_calls(calls: &[(&str, &str, &str)]) -> Completion {
        Completion {
            tool_calls: calls
                .iter()
                .map(|(id, name, args)| ToolCall::new(*id, *name, *args))
                .collect(),
            finish_reason: "tool_calls".into(),
            prompt_tokens: 100,
            ..Default::default()
        }
    }

    /// A completion with no content and no tool calls.
    ///
    /// The failure this reproduces: at high context the model reasons correctly
    /// but loops in its reasoning channel until the token limit, emitting nothing.
    /// Treating that as a clean finish produces a silent no-op turn.
    pub fn empty() -> Completion {
        Completion {
            finish_reason: "stop".into(),
            prompt_tokens: 100,
            ..Default::default()
        }
    }

    /// A completion cut off by the token limit rather than finished.
    pub fn truncated(partial: &str) -> Completion {
        Completion {
            content: partial.into(),
            finish_reason: "length".into(),
            prompt_tokens: 100,
            ..Default::default()
        }
    }

    /// A tool call that also reports a prompt size, for driving the context
    /// ceiling — which only bites on a turn that wants to continue.
    pub fn tool_call_at(id: &str, name: &str, args: &str, prompt_tokens: i64) -> Completion {
        Completion {
            tool_calls: vec![ToolCall::new(id, name, args)],
            finish_reason: "tool_calls".into(),
            prompt_tokens,
            ..Default::default()
        }
    }

    /// An answer that also reports a prompt size, for driving budget ceilings.
    pub fn answer_at(text: &str, prompt_tokens: i64) -> Completion {
        Completion {
            content: text.into(),
            finish_reason: "stop".into(),
            prompt_tokens,
            ..Default::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncation_is_detected_from_finish_reason() {
        let c = Completion {
            finish_reason: "length".into(),
            ..Default::default()
        };
        assert!(c.was_truncated());
    }

    #[test]
    fn a_completion_with_only_reasoning_counts_as_empty() {
        let c = Completion {
            reasoning: "Done. Proceeds. Done. Proceeds.".into(),
            content: String::new(),
            ..Default::default()
        };
        assert!(c.is_empty(), "reasoning alone is not an answer");
    }

    #[test]
    fn a_completion_with_tool_calls_is_not_empty() {
        let c = Completion {
            tool_calls: vec![ToolCall::new("1", "ls", "{}")],
            ..Default::default()
        };
        assert!(!c.is_empty());
    }
}
