//! Shared SSE line parsing for OpenAI-compatible streaming endpoints (LM Studio, OpenRouter, etc.).

use serde_json::Value;
use smithy_tools::ToolCall;

use crate::provider::{Completion, Delta};

#[derive(Default, Debug, Clone)]
pub struct PartialCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
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
