//! Shared SSE line parsing for OpenAI-compatible streaming endpoints (LM Studio, OpenRouter, etc.).

use std::time::Duration;

use futures_util::StreamExt;
use serde_json::Value;
use smithy_tools::ToolCall;

use crate::provider::{Completion, Delta, ProviderError};

/// Hosted endpoints: 90s of socket silence is a hang. Do not use this on a
/// local engine — llama.cpp can decode for minutes without flushing SSE
/// (prefill, then again while buffering a large `write` body). The turn
/// clock is the hang bound there.
pub const STREAM_IDLE: Duration = Duration::from_secs(90);

/// How long to wait for SSE bytes.
#[derive(Clone, Copy, Debug)]
pub struct StreamIdle {
    /// Until the first chunk. Local prefill is silent for this whole window.
    pub until_first: Duration,
    /// Between chunks once tokens have started.
    pub between: Duration,
}

impl StreamIdle {
    /// Cloud endpoints start streaming promptly. The same bound both sides.
    pub const fn stall() -> Self {
        Self {
            until_first: STREAM_IDLE,
            between: STREAM_IDLE,
        }
    }

    /// Local: no second, shorter idle. Prefill and mid-stream gaps share the
    /// request/turn timeout. A quiet socket is not evidence the GPU stopped.
    pub fn local(timeout: Duration) -> Self {
        Self {
            until_first: timeout,
            between: timeout,
        }
    }
}

#[derive(Default, Debug, Clone)]
pub struct PartialCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

/// Cached prompt tokens from a usage object, tolerating the three field names
/// providers actually emit.
///
/// Order is deliberate: the nested OpenAI-compatible shape is checked first,
/// then DeepSeek's top-level hit count, then Anthropic's cache-read name.
/// Returning the first present value (including zero) matters — a reported
/// zero is "cache miss", not "field absent."
pub fn cached_tokens_from_usage(usage: &Value) -> Option<i64> {
    if let Some(n) = usage["prompt_tokens_details"]["cached_tokens"].as_i64() {
        return Some(n);
    }
    if let Some(n) = usage["prompt_cache_hit_tokens"].as_i64() {
        return Some(n);
    }
    if let Some(n) = usage["cache_read_input_tokens"].as_i64() {
        return Some(n);
    }
    None
}

/// Apply one `data:` line. Returns true when the stream is finished.
pub fn apply_sse_line(
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
        if let Some(r) = v["usage"]["completion_tokens_details"]["reasoning_tokens"].as_i64() {
            out.reasoning_tokens = r;
        }
        // Providers disagree on the field name. First one present wins —
        // measured shapes: OpenAI-compatible / OpenRouter
        // (`prompt_tokens_details.cached_tokens`), DeepSeek
        // (`prompt_cache_hit_tokens`), Anthropic-shaped
        // (`cache_read_input_tokens`). Dropping all of them was why the cost
        // meter billed cache hits at the cold rate and why a broken prefix
        // never showed up as a collapsed hit rate.
        out.cached_tokens = cached_tokens_from_usage(&v["usage"]).unwrap_or(out.cached_tokens);
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

pub fn build_tool_calls(partials: Vec<PartialCall>) -> Vec<ToolCall> {
    partials
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
        .collect()
}

/// Drain an OpenAI-style SSE body into a [`Completion`].
///
/// Shared by LM Studio, OpenRouter and DeepSeek so a stall is handled once.
pub async fn consume_sse_stream<S, B, E>(
    stream: S,
    on_delta: Option<&(dyn Fn(Delta) + Send + Sync)>,
) -> Result<Completion, ProviderError>
where
    S: futures_util::Stream<Item = Result<B, E>>,
    B: AsRef<[u8]>,
    E: std::fmt::Display,
{
    consume_sse_with_idle(stream, on_delta, StreamIdle::stall()).await
}

pub async fn consume_sse_with_idle<S, B, E>(
    stream: S,
    on_delta: Option<&(dyn Fn(Delta) + Send + Sync)>,
    idle: StreamIdle,
) -> Result<Completion, ProviderError>
where
    S: futures_util::Stream<Item = Result<B, E>>,
    B: AsRef<[u8]>,
    E: std::fmt::Display,
{
    tokio::pin!(stream);
    let mut out = Completion::default();
    let mut partials: Vec<PartialCall> = Vec::new();
    let mut buffer = String::new();
    let mut waiting_first = true;

    loop {
        let wait = if waiting_first {
            idle.until_first
        } else {
            idle.between
        };
        let chunk = match tokio::time::timeout(wait, stream.next()).await {
            Ok(Some(Ok(chunk))) => chunk,
            Ok(Some(Err(e))) => {
                return Err(ProviderError::BadResponse(format!(
                    "stream read error: {e}"
                )));
            }
            Ok(None) => break,
            Err(_) => {
                let msg = if waiting_first {
                    "the model sent no tokens (prefill or stall)".to_string()
                } else {
                    "the model stopped sending tokens (stream stalled)".to_string()
                };
                return Err(ProviderError::BadResponse(msg));
            }
        };
        waiting_first = false;
        buffer.push_str(&String::from_utf8_lossy(chunk.as_ref()));

        let mut done = false;
        while let Some(newline) = buffer.find('\n') {
            let line = buffer[..newline].trim().to_string();
            buffer.drain(..=newline);
            if apply_sse_line(&line, &mut out, &mut partials, on_delta) {
                done = true;
            }
        }
        if done {
            break;
        }
    }

    out.tool_calls = build_tool_calls(partials);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt;

    fn idle(until_first: Duration, between: Duration) -> StreamIdle {
        StreamIdle {
            until_first,
            between,
        }
    }

    #[tokio::test]
    async fn a_silent_stream_fails_rather_than_sitting_forever() {
        let stream = futures_util::stream::pending::<Result<Vec<u8>, String>>();
        let err = consume_sse_with_idle(
            stream,
            None,
            idle(Duration::from_millis(20), Duration::from_secs(5)),
        )
        .await
        .unwrap_err();
        assert!(
            err.to_string().contains("stall"),
            "a hung body has to surface as a failed turn, not working… forever: {err}"
        );
    }

    #[tokio::test]
    async fn a_slow_first_chunk_is_prefill_not_a_stall() {
        let frames = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n",
            "data: [DONE]\n",
        )
        .to_string();
        let stream = futures_util::stream::once(async move {
            tokio::time::sleep(Duration::from_millis(40)).await;
            Ok::<_, String>(frames.into_bytes())
        });
        let out = consume_sse_with_idle(
            stream,
            None,
            idle(Duration::from_millis(200), Duration::from_millis(10)),
        )
        .await
        .expect("prefill silence must not abort the stream");
        assert_eq!(out.content, "hi");
    }

    #[tokio::test]
    async fn silence_after_the_first_chunk_is_a_stall() {
        let first = b"data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n".to_vec();
        let stream = futures_util::stream::once(async { Ok::<_, String>(first) })
            .chain(futures_util::stream::pending::<Result<Vec<u8>, String>>());
        let err = consume_sse_with_idle(
            stream,
            None,
            idle(Duration::from_secs(5), Duration::from_millis(20)),
        )
        .await
        .unwrap_err();
        assert!(
            err.to_string().contains("stopped sending"),
            "generation silence is still a stall: {err}"
        );
    }

    #[tokio::test]
    async fn a_complete_sse_body_is_parsed() {
        let frames = concat!(
            "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"think\"}}]}\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n",
            "data: [DONE]\n",
        );
        let stream = futures_util::stream::iter([Ok::<_, String>(frames.as_bytes().to_vec())]);
        let out = consume_sse_stream(stream, None).await.expect("parses");
        assert_eq!(out.reasoning, "think");
        assert_eq!(out.content, "hi");
    }
}
