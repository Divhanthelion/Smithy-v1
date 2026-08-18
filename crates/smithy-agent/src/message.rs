//! Conversation history.
//!
//! Ordinary turns only [`History::push`]. Quietly editing an early turn misses
//! the model's cached prefix and forces a full cold prefill — minutes, not
//! milliseconds, on a local endpoint. Two explicit exceptions rewrite bytes
//! that were already sent:
//!
//! - [`History::compacted`]: Compact installs a new prefix on purpose.
//! - [`History::stub_superseded_file`]: after a write lands, prior `read` /
//!   `edit` / `write` payloads for that path are lies. Disk is the source of
//!   truth. Providers with a strict local KV (LM Studio) leave this off.

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

    /// Serialize to an OpenAI-style message object, including any reasoning
    /// trace. Inspection of a stored Session uses this; the POST body uses
    /// [`Self::to_api_with_reasoning`] so providers that 400 without the trace
    /// can opt in without forcing every endpoint to carry it.
    pub fn to_api(&self) -> Value {
        self.to_api_with_reasoning(true)
    }

    pub fn to_api_with_reasoning(&self, include_reasoning: bool) -> Value {
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
        if include_reasoning && !self.reasoning.is_empty() {
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
/// `insert` / `get_mut` for general editing: quietly rewriting an early turn
/// misses the prefix cache. [`Self::compacted`] and [`Self::stub_superseded_file`]
/// are the explicit exceptions.
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

    /// Serialize the whole history to the `messages` array, including reasoning
    /// traces. Persist and tests use this.
    pub fn to_api(&self) -> Value {
        self.to_api_with_reasoning(true)
    }

    /// Same as [`Self::to_api`], optionally omitting `reasoning_content`.
    ///
    /// DeepSeek V4 400s on the next request unless a tool-calling assistant
    /// message round-trips its thinking. LM Studio does not need that, and
    /// sending tens of thousands of thinking tokens is sludge.
    pub fn to_api_with_reasoning(&self, include_reasoning: bool) -> Value {
        Value::Array(
            self.messages
                .iter()
                .map(|m| m.to_api_with_reasoning(include_reasoning))
                .collect(),
        )
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

    /// After a write to `path` has landed, prior file snapshots of that path are
    /// stale. Replace `read` results and `edit`/`write` arguments with a stub so
    /// the next completion does not attend to a lie. Tool-call ids stay, so the
    /// conversation remains well-formed.
    ///
    /// This rewrites earlier messages. A local KV cache will miss. Callers that
    /// must keep the prefix (LM Studio) should not call this.
    pub fn stub_superseded_file(&mut self, path: &str) {
        let ids: Vec<String> = self
            .messages
            .iter()
            .filter(|m| m.role == Role::Assistant)
            .flat_map(|m| m.tool_calls.iter())
            .filter(|c| is_file_snapshot_tool(&c.name) && path_in_arguments(&c.arguments, path))
            .map(|c| c.id.clone())
            .collect();
        if ids.is_empty() {
            return;
        }
        for message in &mut self.messages {
            if message.role == Role::Assistant {
                for call in &mut message.tool_calls {
                    if ids.iter().any(|id| id == &call.id) {
                        call.arguments = stub_file_arguments(path);
                    }
                }
            }
            if message.role == Role::Tool
                && message.tool_name.as_deref() == Some("read")
                && message
                    .tool_call_id
                    .as_ref()
                    .is_some_and(|id| ids.iter().any(|kept| kept == id))
            {
                message.content = format!(
                    "[contents of `{path}` omitted — a later write landed; re-read if you need the current file]"
                );
            }
        }
    }

    /// Drop `content` / `old_string` / `new_string` from a write or edit that
    /// just landed. Those bytes were generated as output, not yet part of any
    /// cached prefix; omitting them before the next request keeps them out of
    /// History without rewriting an earlier turn.
    pub fn redact_landed_write_args(&mut self, call_id: &str, path: &str) {
        for message in self.messages.iter_mut().rev() {
            if message.role != Role::Assistant {
                continue;
            }
            for call in &mut message.tool_calls {
                if call.id == call_id && is_write_tool(&call.name) {
                    call.arguments = stub_file_arguments(path);
                    return;
                }
            }
        }
    }
}

fn is_file_snapshot_tool(name: &str) -> bool {
    matches!(name, "read" | "write" | "edit")
}

fn is_write_tool(name: &str) -> bool {
    matches!(name, "write" | "edit")
}

fn stub_file_arguments(path: &str) -> String {
    json!({
        "path": path,
        "_omitted": "superseded by a later write; re-read the file"
    })
    .to_string()
}

fn path_in_arguments(arguments: &str, path: &str) -> bool {
    let Ok(value) = serde_json::from_str::<Value>(arguments) else {
        return false;
    };
    value
        .get("path")
        .and_then(|v| v.as_str())
        .is_some_and(|p| paths_match(p, path))
}

fn paths_match(a: &str, b: &str) -> bool {
    let a = normalize_path(a);
    let b = normalize_path(b);
    a == b || a.ends_with(&format!("/{b}")) || b.ends_with(&format!("/{a}"))
}

fn normalize_path(path: &str) -> String {
    let path = path.replace('\\', "/");
    let path = path.trim_end_matches('/');
    path.strip_prefix("./").unwrap_or(path).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use smithy_tools::{ToolCall, ToolResult};

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
        let msg = Message::assistant_with_calls("", vec![ToolCall::new("call_1", "read", "{}")])
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

    #[test]
    fn reasoning_can_be_omitted_from_the_wire_body() {
        let msg = Message::assistant_with_calls("", vec![ToolCall::new("call_1", "read", "{}")])
            .with_reasoning("need the file first");
        let with = msg.to_api_with_reasoning(true);
        let without = msg.to_api_with_reasoning(false);
        assert_eq!(with["reasoning_content"], "need the file first");
        assert!(
            without.get("reasoning_content").is_none(),
            "LM Studio must not be sent the thinking trace: {without}"
        );
        assert_eq!(without["tool_calls"][0]["id"], "call_1");
    }

    #[test]
    fn a_later_write_stubs_the_stale_read_of_that_file() {
        let mut h = History::new("sys");
        h.push(Message::user("fix it"));
        let read = ToolCall::new("r1", "read", r#"{"path":"src/lib.rs"}"#);
        h.push(Message::assistant_with_calls("", vec![read.clone()]));
        h.push(Message::tool_result(&ToolResult::ok(
            &read,
            "fn old() {}\n".repeat(50),
        )));
        let write = ToolCall::new(
            "w1",
            "write",
            r#"{"path":"src/lib.rs","content":"fn new() {}"}"#,
        );
        h.push(Message::assistant_with_calls("", vec![write.clone()]));
        h.push(Message::tool_result(&ToolResult::ok(
            &write,
            "Overwrote `src/lib.rs` (1 line).",
        )));

        h.stub_superseded_file("src/lib.rs");

        let api = h.to_api().to_string();
        assert!(
            !api.contains("fn old()"),
            "stale file body must not be sent: {api}"
        );
        assert!(
            !api.contains("fn new()"),
            "the write payload is on disk, not in History: {api}"
        );
        assert!(
            api.contains("omitted"),
            "the stub has to say why the body is gone: {api}"
        );
        assert!(
            api.contains("Overwrote"),
            "the short write result stays: {api}"
        );
        assert!(api.contains("r1") && api.contains("w1"), "ids stay");
    }

    #[test]
    fn stubbing_one_path_leaves_another_file_alone() {
        let mut h = History::new("sys");
        let keep = ToolCall::new("r1", "read", r#"{"path":"a.rs"}"#);
        let drop = ToolCall::new("r2", "read", r#"{"path":"b.rs"}"#);
        h.push(Message::assistant_with_calls(
            "",
            vec![keep.clone(), drop.clone()],
        ));
        h.push(Message::tool_result(&ToolResult::ok(&keep, "KEEPME")));
        h.push(Message::tool_result(&ToolResult::ok(&drop, "DROPM")));
        h.stub_superseded_file("b.rs");
        let api = h.to_api().to_string();
        assert!(api.contains("KEEPME"), "{api}");
        assert!(!api.contains("DROPM"), "{api}");
    }

    #[test]
    fn relative_and_dotted_paths_are_the_same_file() {
        let mut h = History::new("sys");
        let read = ToolCall::new("r1", "read", r#"{"path":"./src/lib.rs"}"#);
        h.push(Message::assistant_with_calls("", vec![read.clone()]));
        h.push(Message::tool_result(&ToolResult::ok(&read, "STALE")));
        h.stub_superseded_file("src/lib.rs");
        assert!(!h.to_api().to_string().contains("STALE"), "{}", h.to_api());
    }

    #[test]
    fn redact_landed_write_drops_only_that_call_s_payload() {
        let mut h = History::new("sys");
        let read = ToolCall::new("r1", "read", r#"{"path":"a.rs"}"#);
        h.push(Message::assistant_with_calls("", vec![read.clone()]));
        h.push(Message::tool_result(&ToolResult::ok(&read, "OLDFILE")));
        let write = ToolCall::new("w1", "write", r#"{"path":"a.rs","content":"NEWFILE"}"#);
        h.push(Message::assistant_with_calls("", vec![write.clone()]));
        h.push(Message::tool_result(&ToolResult::ok(&write, "Overwrote")));
        h.redact_landed_write_args("w1", "a.rs");
        let api = h.to_api().to_string();
        assert!(api.contains("OLDFILE"), "LM Studio keeps the prefix: {api}");
        assert!(!api.contains("NEWFILE"), "new bytes stay on disk: {api}");
    }
}
