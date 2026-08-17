//! Conversation history.
//!
//! The central invariant: **history is append-only**. Nothing here offers a way
//! to mutate, reorder, or re-render an earlier turn, because doing so
//! invalidates the model's cached prefix and forces a full cold prefill — which
//! on a local endpoint at real context sizes costs minutes, not milliseconds.

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use smithy_tools::{ToolCall, ToolResult};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    /// A tool result being reported back to the model.
    Tool,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
        }
    }
}

/// One entry in the history.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: String,
    /// Tool calls this assistant turn requested.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    /// For [`Role::Tool`]: the id of the call this answers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// For [`Role::Tool`]: the name of the tool that ran.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    /// Thinking-mode trace for this assistant message.
    ///
    /// DeepSeek V4 (and any endpoint that advertised `tools`) rejects the next
    /// request with 400 unless this comes back on the same assistant message
    /// that issued the tool calls. Empty for ordinary answers — those must not
    /// grow the prefix. Shown live via [`crate::provider::Delta::Reasoning`];
    /// this field is only the round-trip, not the Session's reasoning log.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub reasoning: String,
}

impl Message {
    fn bare(role: Role, content: impl Into<String>) -> Message {
        Message {
            role,
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: None,
            tool_name: None,
            reasoning: String::new(),
        }
    }

    pub fn system(content: impl Into<String>) -> Message {
        Message::bare(Role::System, content)
    }

    pub fn user(content: impl Into<String>) -> Message {
        Message::bare(Role::User, content)
    }

    pub fn assistant(content: impl Into<String>) -> Message {
        Message::bare(Role::Assistant, content)
    }

    pub fn assistant_with_calls(content: impl Into<String>, calls: Vec<ToolCall>) -> Message {
        Message {
            tool_calls: calls,
            ..Message::bare(Role::Assistant, content)
        }
    }

    /// Attach a thinking-mode trace. See [`Message::reasoning`].
    pub fn with_reasoning(mut self, reasoning: impl Into<String>) -> Message {
        self.reasoning = reasoning.into();
        self
    }

    /// A tool result, correlated to the call it answers.
    ///
    /// coda wrapped results in `<tool_response>` tags on a `user` message and
    /// relied on ordering to associate them with calls. That works until the
    /// model issues parallel calls, at which point nothing prevents result *n*
    /// being read as the answer to call *m*. Carrying the id removes the
    /// ambiguity entirely.
    pub fn tool_result(result: &ToolResult) -> Message {
        Message {
            role: Role::Tool,
            content: result.content.clone(),
            tool_calls: Vec::new(),
            tool_call_id: Some(result.tool_call_id.clone()),
            tool_name: Some(result.name.clone()),
            reasoning: String::new(),
        }
    }

    /// Serialize to an OpenAI-style message object.
    pub fn to_api(&self) -> Value {
        let mut m = Map::new();
        m.insert("role".into(), Value::String(self.role.as_str().into()));
        m.insert("content".into(), Value::String(self.content.clone()));

        if !self.tool_calls.is_empty() {
            let calls: Vec<Value> = self
                .tool_calls
                .iter()
                .map(|c| {
                    json!({
                        "id": c.id,
                        "type": "function",
                        "function": { "name": c.name, "arguments": c.arguments }
                    })
                })
                .collect();
            m.insert("tool_calls".into(), Value::Array(calls));
        }
        if !self.reasoning.is_empty() {
            m.insert(
                "reasoning_content".into(),
                Value::String(self.reasoning.clone()),
            );
        }
        if let Some(id) = &self.tool_call_id {
            m.insert("tool_call_id".into(), Value::String(id.clone()));
        }
        if let Some(name) = &self.tool_name {
            m.insert("name".into(), Value::String(name.clone()));
        }
        Value::Object(m)
    }
}

/// The conversation this Session will send on the next completion.
///
/// Ordinary turns only [`Self::push`]. There is no `remove` / `truncate` /
/// `insert` / `get_mut`: quietly editing an early turn would miss the prefix
/// cache while pretending the old prefix was still there. [`Self::compacted`]
/// is the explicit exception — Compact installs a new prefix on purpose.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct History {
    messages: Vec<Message>,
}

impl History {
    pub fn new(system_prompt: impl Into<String>) -> History {
        History {
            messages: vec![Message::system(system_prompt)],
        }
    }

    pub fn push(&mut self, message: Message) {
        self.messages.push(message);
    }

    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    pub fn len(&self) -> usize {
        self.messages.len()
    }

    pub fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }

    /// The system prompt, which sits at the head of the cached prefix.
    pub fn system_prompt(&self) -> Option<&str> {
        self.messages
            .first()
            .filter(|m| m.role == Role::System)
            .map(|m| m.content.as_str())
    }

    /// Serialize the whole history to the `messages` array.
    pub fn to_api(&self) -> Value {
        Value::Array(self.messages.iter().map(|m| m.to_api()).collect())
    }

    /// Restore a history from persisted messages.
    ///
    /// Used by [`crate::persist`] and by Compact. The messages go back verbatim.
    pub fn from_messages(messages: Vec<Message>) -> History {
        History { messages }
    }

    /// A new prefix: the same system prompt and one user message (the summary).
    ///
    /// Compact's job. The old turns are not in this History; persist may keep a
    /// `.full.json` sidecar so the log is still on disk.
    pub fn compacted(system_prompt: impl Into<String>, summary: impl Into<String>) -> History {
        History {
            messages: vec![Message::system(system_prompt), Message::user(summary)],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smithy_tools::ToolCall;

    #[test]
    fn system_prompt_heads_the_history() {
        let h = History::new("you are smithy");
        assert_eq!(h.system_prompt(), Some("you are smithy"));
        assert_eq!(h.len(), 1);
    }

    #[test]
    fn compacted_history_is_system_plus_summary() {
        let h = History::compacted("sys", "the summary");
        assert_eq!(h.len(), 2);
        assert_eq!(h.system_prompt(), Some("sys"));
        assert_eq!(h.messages()[1].content, "the summary");
    }

    #[test]
    fn serializes_an_assistant_tool_call() {
        let mut h = History::new("sys");
        h.push(Message::assistant_with_calls(
            "",
            vec![ToolCall::new("call_1", "read", r#"{"path":"a.rs"}"#)],
        ));
        let api = h.to_api();
        let call = &api[1]["tool_calls"][0];
        assert_eq!(call["id"], "call_1");
        assert_eq!(call["type"], "function");
        assert_eq!(call["function"]["name"], "read");
        assert!(
            api[1].get("reasoning_content").is_none(),
            "an empty trace must not grow the prefix"
        );
    }

    /// DeepSeek V4 returns 400 on the next request unless a tool-calling
    /// assistant message carries the thinking that produced the calls.
    #[test]
    fn a_tool_call_round_trips_its_reasoning_trace() {
        let msg = Message::assistant_with_calls(
            "",
            vec![ToolCall::new("call_1", "read", "{}")],
        )
        .with_reasoning("need the file first");
        let api = msg.to_api();
        assert_eq!(api["reasoning_content"], "need the file first");
        assert_eq!(api["tool_calls"][0]["id"], "call_1");
    }

    /// The correlation coda lacked: a result names the call it answers.
    #[test]
    fn tool_results_carry_their_call_id() {
        let call = ToolCall::new("call_7", "read", "{}");
        let result = ToolResult::ok(&call, "file contents");
        let msg = Message::tool_result(&result);
        assert_eq!(msg.role, Role::Tool);
        assert_eq!(msg.tool_call_id.as_deref(), Some("call_7"));
        assert_eq!(msg.tool_name.as_deref(), Some("read"));
        let api = msg.to_api();
        assert_eq!(api["tool_call_id"], "call_7");
        assert_eq!(api["role"], "tool");
    }

    /// Two parallel calls must map to two distinguishable results.
    #[test]
    fn parallel_tool_results_stay_distinguishable() {
        let a = ToolCall::new("call_a", "read", "{}");
        let b = ToolCall::new("call_b", "grep", "{}");
        let ra = Message::tool_result(&ToolResult::ok(&a, "A"));
        let rb = Message::tool_result(&ToolResult::ok(&b, "B"));
        assert_ne!(ra.tool_call_id, rb.tool_call_id);
    }

    #[test]
    fn serialization_is_deterministic() {
        let mut h = History::new("sys");
        h.push(Message::user("hello"));
        h.push(Message::assistant("hi"));
        assert_eq!(
            serde_json::to_string(&h.to_api()).unwrap(),
            serde_json::to_string(&h.to_api()).unwrap()
        );
    }

    #[test]
    fn round_trips_through_serde_unchanged() {
        let mut h = History::new("sys");
        h.push(Message::user("hello"));
        h.push(Message::assistant_with_calls(
            "thinking",
            vec![ToolCall::new("c1", "ls", "{}")],
        ));
        let json = serde_json::to_string(&h).unwrap();
        let back: History = serde_json::from_str(&json).unwrap();
        assert_eq!(
            serde_json::to_string(&h.to_api()).unwrap(),
            serde_json::to_string(&back.to_api()).unwrap(),
            "a restored history must serialize byte-identically, or resuming \
             costs a full cold prefill"
        );
    }
}
