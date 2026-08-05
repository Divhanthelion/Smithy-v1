//! The tool trait, the registry, and the single dispatch choke point.
//!
//! coda's rule is preserved verbatim and is the most important invariant here:
//!
//! > **`Registry::execute` is the single tool-dispatch choke point** — the seam
//! > for future pre/post hooks and kernel sandboxing. Keep it the only dispatch
//! > path.
//!
//! [`ToolHook`] is that seam, made real. divcli reserved a `hooks/` module for
//! it and left the file empty; forge implemented the same idea three separate
//! times (write review, shell approval, tool-progress events) hard-wired into
//! its chat service. Here there is one interface, and the UI, the guardrails and
//! the security scanner all attach through it without any tool knowing they
//! exist.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;

use crate::sandbox::Workspace;
use crate::schema::{ToolCall, ToolDefinition, ToolOutput, ToolResult};

/// One entry in the session-scoped plan maintained by the `todo` tool.
#[derive(Clone, Debug, PartialEq)]
pub struct Todo {
    pub content: String,
    pub status: String,
}

/// Everything a tool is allowed to reach.
///
/// Deliberately minimal: a tool gets the workspace capability and the shared
/// todo list, and nothing else. It cannot reach the model, the UI, or the
/// filesystem outside the workspace.
///
/// `Mutex` rather than coda's `RefCell` because tools now run on an async
/// runtime and the context is shared across tasks.
pub struct ToolCtx {
    pub workspace: Workspace,
    pub todos: Mutex<Vec<Todo>>,
}

/// Call-local authorization checked at irreversible boundaries.
///
/// Hooks can attach guards without mutating shared `ToolCtx`; that keeps
/// concurrent calls independent and is the seam the broader execution-control
/// work can extend beyond file publication.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ExecutionToken {
    pub generation: u64,
    pub turn: u64,
}

impl ExecutionToken {
    pub const fn new(generation: u64, turn: u64) -> Self {
        Self { generation, turn }
    }
}

/// The cancellation and one absolute deadline governing one turn.
///
/// A control is created once when the turn is claimed and cloned through every
/// await. Deriving a fresh timeout after an approval or provider call would let
/// each wait spend the full budget and turn a fifteen-minute ceiling into hours.
#[derive(Clone, Default)]
pub struct ExecutionControl {
    token: Option<ExecutionToken>,
    cancellation: Option<tokio_util::sync::CancellationToken>,
    deadline: Option<tokio::time::Instant>,
    publication_guards: Vec<Arc<dyn Fn() -> Result<(), String> + Send + Sync>>,
}

#[derive(Clone)]
pub struct StopLease {
    token: ExecutionToken,
    cancellation: tokio_util::sync::CancellationToken,
}

impl std::fmt::Debug for ExecutionControl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExecutionControl")
            .field("token", &self.token)
            .field("deadline", &self.deadline)
            .field("publication_guards", &self.publication_guards.len())
            .finish()
    }
}

impl ExecutionControl {
    pub fn for_turn(
        token: ExecutionToken,
        timeout: Duration,
    ) -> (ExecutionControl, StopLease) {
        Self::with_deadline(token, tokio::time::Instant::now() + timeout)
    }

    pub fn with_deadline(
        token: ExecutionToken,
        deadline: tokio::time::Instant,
    ) -> (ExecutionControl, StopLease) {
        let cancellation = tokio_util::sync::CancellationToken::new();
        (
            ExecutionControl {
                token: Some(token),
                cancellation: Some(cancellation.clone()),
                deadline: Some(deadline),
                publication_guards: Vec::new(),
            },
            StopLease {
                token,
                cancellation,
            },
        )
    }

    pub fn token(&self) -> Option<ExecutionToken> {
        self.token
    }

    pub fn deadline(&self) -> Option<tokio::time::Instant> {
        self.deadline
    }

    /// Keep the parent's stop identity while tightening a nested operation.
    pub fn bounded_by(&self, timeout: Duration) -> Self {
        let own = tokio::time::Instant::now() + timeout;
        let mut child = self.clone();
        child.deadline = Some(self.deadline.map_or(own, |parent| parent.min(own)));
        child
    }

    pub fn with_publication_guard(
        guard: impl Fn() -> Result<(), String> + Send + Sync + 'static,
    ) -> Self {
        Self {
            token: None,
            cancellation: None,
            deadline: None,
            publication_guards: vec![Arc::new(guard)],
        }
    }

    fn extend(&mut self, other: Self) {
        self.publication_guards.extend(other.publication_guards);
    }

    pub fn authorize_publication(&self) -> Result<(), String> {
        self.check()?;
        for guard in &self.publication_guards {
            guard()?;
        }
        Ok(())
    }

    pub fn check(&self) -> Result<(), String> {
        if self
            .cancellation
            .as_ref()
            .is_some_and(|token| token.is_cancelled())
        {
            return Err("stopped by user".into());
        }
        if self
            .deadline
            .is_some_and(|deadline| tokio::time::Instant::now() >= deadline)
        {
            return Err("turn deadline reached".into());
        }
        Ok(())
    }

    pub async fn cancelled(&self) -> String {
        match (&self.cancellation, self.deadline) {
            (Some(cancellation), Some(deadline)) => tokio::select! {
                biased;
                _ = cancellation.cancelled() => "stopped by user".into(),
                _ = tokio::time::sleep_until(deadline) => "turn deadline reached".into(),
            },
            (Some(cancellation), None) => {
                cancellation.cancelled().await;
                "stopped by user".into()
            }
            (None, Some(deadline)) => {
                tokio::time::sleep_until(deadline).await;
                "turn deadline reached".into()
            }
            (None, None) => std::future::pending().await,
        }
    }
}

impl StopLease {
    pub fn token(&self) -> ExecutionToken {
        self.token
    }

    pub fn stop(&self) {
        self.cancellation.cancel();
    }
}

impl ToolCtx {
    pub fn new(workspace: Workspace) -> Self {
        Self {
            workspace,
            todos: Mutex::new(Vec::new()),
        }
    }

    pub fn todos(&self) -> Vec<Todo> {
        self.todos.lock().map(|t| t.clone()).unwrap_or_default()
    }
}

/// A callable tool.
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &'static str;

    /// The schema advertised to the model. Must be constant for the lifetime of
    /// a session — see [`crate::schema`] on prefix-cache stability.
    fn definition(&self) -> ToolDefinition;

    async fn run(&self, args: &Value, ctx: &ToolCtx) -> ToolOutput;

    async fn run_controlled(
        &self,
        args: &Value,
        ctx: &ToolCtx,
        _control: &ExecutionControl,
    ) -> ToolOutput {
        self.run(args, ctx).await
    }

    /// Whether cancellation must wait for this tool's cleanup path.
    ///
    /// Most futures are safe to drop at Registry's select. A subprocess is not:
    /// its blocking worker must kill and reap the process group before the turn
    /// is allowed to report Stopped.
    fn owns_cancellation_cleanup(&self) -> bool {
        false
    }
}

/// What a [`ToolHook`] decided about a pending call.
#[derive(Debug, Clone)]
pub enum HookDecision {
    /// Proceed to the tool.
    Allow,
    /// Proceed with call-local guards checked again at irreversible boundaries.
    AllowWithControl(ExecutionControl),
    /// Refuse. The reason is fed back to the model as an error result, so it can
    /// choose a different approach rather than silently stalling.
    Deny(String),
    /// The hook did the work itself. The message is returned as a **successful**
    /// result and the tool is not run.
    ///
    /// Added for write review. Before it, a hook could only say "no", so an edit
    /// the user had approved and which the UI had already written to disk still
    /// came back to the model as an error — and the model, reasonably, went
    /// looking for what had gone wrong. A session was measured spending 26 of
    /// its 76 tool calls re-editing and polling files whose edits had in fact
    /// landed. Success needs to be expressible.
    Fulfilled(String),
}

/// A pre/post interceptor around every tool call.
///
/// Hooks run in registration order. The first `Deny` short-circuits: the tool
/// never runs and no later hook's `before` is consulted.
///
/// Intended implementors:
/// - a write-review hook that routes proposed file writes to forge's diff modal
/// - an approval hook that suspends on shell commands until the user answers
/// - a secret/SAST scanner (from kimi-sec) over proposed content
/// - a telemetry hook logging every call to the experiment database
#[async_trait]
pub trait ToolHook: Send + Sync {
    fn name(&self) -> &'static str;

    /// Called before dispatch. Default: allow.
    async fn before(&self, _call: &ToolCall, _args: &Value, _ctx: &ToolCtx) -> HookDecision {
        HookDecision::Allow
    }

    /// Called after the tool returns, including when a hook denied it. Cannot
    /// change the result — this is for observation only.
    async fn after(&self, _call: &ToolCall, _result: &ToolResult, _ctx: &ToolCtx) {}
}

/// The set of tools available to the model, plus the hooks wrapping them.
pub struct Registry {
    tools: Vec<Box<dyn Tool>>,
    hooks: Vec<Box<dyn ToolHook>>,
}

impl Registry {
    pub fn new() -> Self {
        Self {
            tools: Vec::new(),
            hooks: Vec::new(),
        }
    }

    /// The always-on core set.
    ///
    /// coda measured that there is no "tool cliff" at this count and concluded
    /// tools should never be gated: gating changes the tool block between turns,
    /// which changes the cached prefix, which forces a full cold prefill. Order
    /// is fixed for the same reason.
    pub fn core() -> Self {
        Registry::new()
            .with(crate::tools::read::Read)
            .with(crate::tools::write::Write)
            .with(crate::tools::edit::Edit)
            .with(crate::tools::ls::Ls)
            .with(crate::tools::glob::Glob)
            .with(crate::tools::grep::Grep)
            .with(crate::tools::bash::Bash)
            .with(crate::tools::todo::TodoTool)
    }

    pub fn with(mut self, tool: impl Tool + 'static) -> Self {
        self.tools.push(Box::new(tool));
        self
    }

    pub fn push(&mut self, tool: Box<dyn Tool>) {
        self.tools.push(tool);
    }

    pub fn add_hook(&mut self, hook: Box<dyn ToolHook>) {
        self.hooks.push(hook);
    }

    pub fn with_hook(mut self, hook: impl ToolHook + 'static) -> Self {
        self.hooks.push(Box::new(hook));
        self
    }

    pub fn names(&self) -> Vec<&'static str> {
        self.tools.iter().map(|t| t.name()).collect()
    }

    pub fn definitions(&self) -> Vec<ToolDefinition> {
        self.tools.iter().map(|t| t.definition()).collect()
    }

    /// The OpenAI `tools` array. Built from a fixed tool order so the bytes are
    /// identical on every turn of a session.
    pub fn openai_schemas(&self) -> Value {
        Value::Array(
            self.tools
                .iter()
                .map(|t| t.definition().to_openai())
                .collect(),
        )
    }

    fn get(&self, name: &str) -> Option<&dyn Tool> {
        self.tools
            .iter()
            .find(|t| t.name() == name)
            .map(|b| b.as_ref())
    }

    /// **The only dispatch path.** Everything — argument parsing, hooks, the
    /// tool itself, and post-observation — happens here so nothing can bypass a
    /// guardrail by calling a tool directly.
    pub async fn execute(&self, call: &ToolCall, ctx: &ToolCtx) -> ToolResult {
        self.execute_controlled(call, ctx, &ExecutionControl::default())
            .await
    }

    pub async fn execute_controlled(
        &self,
        call: &ToolCall,
        ctx: &ToolCtx,
        control: &ExecutionControl,
    ) -> ToolResult {
        let result = self.execute_inner(call, ctx, control).await;
        for hook in &self.hooks {
            tokio::select! {
                biased;
                _ = control.cancelled() => break,
                _ = hook.after(call, &result, ctx) => {}
            }
        }
        result
    }

    async fn execute_inner(
        &self,
        call: &ToolCall,
        ctx: &ToolCtx,
        parent_control: &ExecutionControl,
    ) -> ToolResult {
        if let Err(reason) = parent_control.check() {
            return ToolResult::err(call, reason);
        }
        let Some(tool) = self.get(&call.name) else {
            return ToolResult::err(
                call,
                format!(
                    "unknown tool `{}`. Available tools: {}",
                    call.name,
                    self.names().join(", ")
                ),
            );
        };

        let args = match call.parsed_arguments() {
            Ok(v) => v,
            Err(e) => return ToolResult::err(call, e),
        };

        let mut control = parent_control.clone();
        for hook in &self.hooks {
            let decision = tokio::select! {
                biased;
                reason = control.cancelled() => return ToolResult::err(call, reason),
                decision = hook.before(call, &args, ctx) => decision,
            };
            match decision {
                HookDecision::Allow => {}
                HookDecision::AllowWithControl(additional) => control.extend(additional),
                HookDecision::Deny(reason) => {
                    return ToolResult::err(call, format!("`{}` was not run: {reason}", call.name));
                }
                // No `was not run` prefix: from the model's side the call
                // succeeded, and saying otherwise is what sent it hunting.
                HookDecision::Fulfilled(message) => {
                    return ToolResult {
                        tool_call_id: call.id.clone(),
                        name: call.name.clone(),
                        content: message,
                        is_error: false,
                    };
                }
            }
        }
        if let Err(reason) = control.check() {
            return ToolResult::err(call, reason);
        }

        let output = if tool.owns_cancellation_cleanup() {
            tool.run_controlled(&args, ctx, &control).await
        } else {
            tokio::select! {
                biased;
                reason = control.cancelled() => return ToolResult::err(call, reason),
                output = tool.run_controlled(&args, ctx, &control) => output,
            }
        };
        ToolResult {
            tool_call_id: call.id.clone(),
            name: call.name.clone(),
            content: output.content,
            is_error: output.is_error,
        }
    }
}

impl Default for Registry {
    fn default() -> Self {
        Registry::core()
    }
}

/// Middle-truncate text, keeping head and tail.
///
/// Used wherever unbounded output could blow the context budget. Keeping both
/// ends matters: a command's first lines say what ran and its last lines say how
/// it failed, and a head-only truncation throws the error away.
pub fn middle_truncate(s: &str, max: usize) -> String {
    let count = s.chars().count();
    if count <= max {
        return s.to_string();
    }
    let keep = max / 2;
    let head: String = s.chars().take(keep).collect();
    let tail: String = s.chars().skip(count - keep).collect();
    format!(
        "{head}\n\n… [{} chars truncated] …\n\n{tail}",
        count - 2 * keep
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::ToolOutput;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    struct Echo;

    #[async_trait]
    impl Tool for Echo {
        fn name(&self) -> &'static str {
            "echo"
        }
        fn definition(&self) -> ToolDefinition {
            ToolDefinition::new("echo", "Echo the input", vec![])
        }
        async fn run(&self, args: &Value, _ctx: &ToolCtx) -> ToolOutput {
            ToolOutput::ok(
                args.get("text")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
            )
        }
    }

    struct DenyAll;

    #[async_trait]
    impl ToolHook for DenyAll {
        fn name(&self) -> &'static str {
            "deny-all"
        }
        async fn before(&self, _c: &ToolCall, _a: &Value, _x: &ToolCtx) -> HookDecision {
            HookDecision::Deny("policy says no".into())
        }
    }

    struct CountAfter(Arc<AtomicUsize>);

    #[async_trait]
    impl ToolHook for CountAfter {
        fn name(&self) -> &'static str {
            "count-after"
        }
        async fn after(&self, _c: &ToolCall, _r: &ToolResult, _x: &ToolCtx) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn ctx() -> (tempfile::TempDir, ToolCtx) {
        let tmp = tempfile::tempdir().unwrap();
        let ws = Workspace::open(tmp.path()).unwrap();
        (tmp, ToolCtx::new(ws))
    }

    #[tokio::test]
    async fn dispatches_to_the_named_tool() {
        let (_t, ctx) = ctx();
        let reg = Registry::new().with(Echo);
        let call = ToolCall::new("1", "echo", r#"{"text":"hello"}"#);
        let result = reg.execute(&call, &ctx).await;
        assert_eq!(result.content, "hello");
        assert!(!result.is_error);
        assert_eq!(result.tool_call_id, "1");
    }

    #[tokio::test]
    async fn unknown_tool_lists_the_available_ones() {
        let (_t, ctx) = ctx();
        let reg = Registry::new().with(Echo);
        let call = ToolCall::new("1", "nope", "{}");
        let result = reg.execute(&call, &ctx).await;
        assert!(result.is_error);
        assert!(result.content.contains("unknown tool"));
        assert!(result.content.contains("echo"));
    }

    #[tokio::test]
    async fn a_denying_hook_prevents_the_tool_from_running() {
        let (_t, ctx) = ctx();
        let reg = Registry::new().with(Echo).with_hook(DenyAll);
        let call = ToolCall::new("1", "echo", r#"{"text":"hello"}"#);
        let result = reg.execute(&call, &ctx).await;
        assert!(result.is_error);
        assert!(result.content.contains("policy says no"));
        assert!(!result.content.contains("hello"));
    }

    #[tokio::test]
    async fn after_hooks_run_even_when_a_call_is_denied() {
        let (_t, ctx) = ctx();
        let count = Arc::new(AtomicUsize::new(0));
        let reg = Registry::new()
            .with(Echo)
            .with_hook(DenyAll)
            .with_hook(CountAfter(count.clone()));
        let call = ToolCall::new("1", "echo", "{}");
        reg.execute(&call, &ctx).await;
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn malformed_arguments_are_reported_not_panicked() {
        let (_t, ctx) = ctx();
        let reg = Registry::new().with(Echo);
        let call = ToolCall::new("1", "echo", "{broken");
        let result = reg.execute(&call, &ctx).await;
        assert!(result.is_error);
        assert!(result.content.contains("not valid JSON"));
    }

    #[test]
    fn core_registry_schema_is_byte_stable() {
        let reg = Registry::core();
        let a = serde_json::to_string(&reg.openai_schemas()).unwrap();
        let b = serde_json::to_string(&Registry::core().openai_schemas()).unwrap();
        assert_eq!(a, b, "tool schemas must be identical between constructions");
    }

    #[test]
    fn core_registry_has_the_expected_tools_in_a_fixed_order() {
        assert_eq!(
            Registry::core().names(),
            vec!["read", "write", "edit", "ls", "glob", "grep", "bash", "todo"]
        );
    }

    #[test]
    fn middle_truncate_keeps_both_ends() {
        let s = "a".repeat(100) + &"b".repeat(100);
        let out = middle_truncate(&s, 20);
        assert!(out.starts_with("aaaaaaaaaa"));
        assert!(out.ends_with("bbbbbbbbbb"));
        assert!(out.contains("truncated"));
    }

    #[test]
    fn middle_truncate_leaves_short_text_alone() {
        assert_eq!(middle_truncate("short", 100), "short");
    }
}
