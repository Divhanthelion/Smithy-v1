//! Shared SSE line parsing for OpenAI-compatible streaming endpoints (LM Studio, OpenRouter, etc.).

use std::time::Duration;

use futures_util::StreamExt;
use serde_json::Value;
use smithy_tools::ToolCall;

use crate::provider::{Completion, Delta, ProviderError};

/// If the TCP connection stays open but the endpoint sends nothing, the panel
/// sits on `working...` until the request timeout (fifteen minutes). That looks
/// like a crash. Ninety seconds of silence after a 200 is a stall, not thinking
/// — thinking models still emit `reasoning_content` tokens.
pub const STREAM_IDLE: Duration = Duration::from_secs(90);

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
    S: futures_util::Stream<Item = Result<B, E>> + Unpin,
    B: AsRef<[u8]>,
    E: std::fmt::Display,
{
    consume_sse_with_idle(stream, on_delta, STREAM_IDLE).await
}

async fn consume_sse_with_idle<S, B, E>(
    mut stream: S,
    on_delta: Option<&(dyn Fn(Delta) + Send + Sync)>,
    idle: Duration,
) -> Result<Completion, ProviderError>
where
    S: futures_util::Stream<Item = Result<B, E>> + Unpin,
    B: AsRef<[u8]>,
    E: std::fmt::Display,
{
    let mut out = Completion::default();
    let mut partials: Vec<PartialCall> = Vec::new();
    let mut buffer = String::new();

    loop {
        let chunk = match tokio::time::timeout(idle, stream.next()).await {
            Ok(Some(Ok(chunk))) => chunk,
            Ok(Some(Err(e))) => {
                return Err(ProviderError::BadResponse(format!(
                    "stream read error: {e}"
                )));
            }
            Ok(None) => break,
            Err(_) => {
                return Err(ProviderError::BadResponse(
                    "the model stopped sending tokens (stream stalled)".into(),
                ));
            }
        };
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

    #[tokio::test]
    async fn a_silent_stream_fails_rather_than_sitting_forever() {
        let stream = futures_util::stream::pending::<Result<Vec<u8>, String>>();
        let err = consume_sse_with_idle(stream, None, Duration::from_millis(20))
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("stalled"),
            "a hung body has to surface as a failed turn, not working… forever: {err}"
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
