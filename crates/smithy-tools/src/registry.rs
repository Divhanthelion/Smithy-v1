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
use std::time::{Duration, Instant};

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

/// Time spent waiting on the user, which the turn clock must not treat as a
/// runaway loop.
///
/// Review and shell approval hold this while a human decides. [`Clone`] shares
/// the same pause with the agent's wall clock — the type lives here because
/// the hooks that wait are the ones that know the wait started.
///
/// A tool must not touch this. It is a hook seam.
#[derive(Clone, Default)]
pub struct GatePause {
    inner: Arc<Mutex<GatePauseInner>>,
}

#[derive(Default)]
struct GatePauseInner {
    depth: u32,
    paused_at: Option<Instant>,
    accumulated: Duration,
}

/// RAII hold: the clock stays paused until this is dropped, including when the
/// future waiting on the user is cancelled.
pub struct GateHold {
    gate: GatePause,
}

impl Drop for GateHold {
    fn drop(&mut self) {
        self.gate.release();
    }
}

impl GatePause {
    /// Pause until the returned guard is dropped.
    pub fn hold(&self) -> GateHold {
        self.acquire();
        GateHold { gate: self.clone() }
    }

    fn acquire(&self) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if inner.depth == 0 {
            inner.paused_at = Some(Instant::now());
        }
        inner.depth = inner.depth.saturating_add(1);
    }

    fn release(&self) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if inner.depth == 0 {
            return;
        }
        inner.depth -= 1;
        if inner.depth == 0 {
            if let Some(since) = inner.paused_at.take() {
                inner.accumulated += Instant::now().saturating_duration_since(since);
            }
        }
    }

    /// How long this pause has been held, as of `now`.
    ///
    /// An open hold counts the interval through `now` so a budget tick during
    /// a wait sees the wait, not wall time.
    pub fn paused_at(&self, now: Instant) -> Duration {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let mut total = inner.accumulated;
        if let Some(since) = inner.paused_at {
            total += now.saturating_duration_since(since);
        }
        total
    }
}

/// Everything a tool is allowed to reach.
///
/// Deliberately minimal: a tool gets the workspace capability and the shared
/// todo list, and nothing else. It cannot reach the model, the UI, or the
/// filesystem outside the workspace.
///
/// [`Self::gate`] is for hooks, not tools: Review and shell approval pause the
/// turn clock while the user decides. A human reading a diff is not a runaway
/// loop.
///
/// `Mutex` rather than coda's `RefCell` because tools now run on an async
/// runtime and the context is shared across tasks.
pub struct ToolCtx {
    pub workspace: Workspace,
    pub todos: Mutex<Vec<Todo>>,
    pub gate: GatePause,
}

impl ToolCtx {
    pub fn new(workspace: Workspace) -> Self {
        Self {
            workspace,
            todos: Mutex::new(Vec::new()),
            gate: GatePause::default(),
        }
    }

    pub fn todos(&self) -> Vec<Todo> {
        self.todos.lock().map(|t| t.clone()).unwrap_or_default()
    }
}

/// A callable tool.
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;

    /// The schema advertised to the model. Must be constant for the lifetime of
    /// a session — see [`crate::schema`] on prefix-cache stability.
    fn definition(&self) -> ToolDefinition;

    async fn run(&self, args: &Value, ctx: &ToolCtx) -> ToolOutput;
}

/// What a [`ToolHook`] decided about a pending call.
#[derive(Debug, Clone)]
pub enum HookDecision {
    /// Proceed to the tool.
    Allow,
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

    pub fn names(&self) -> Vec<&str> {
        self.tools.iter().map(|t| t.name()).collect()
    }

    /// Keep only the named tools. Unknown names are ignored; hooks are unchanged.
    ///
    /// Used when a Session is rebuilt with a Skill `tools:` allowlist, and when
    /// resume replays a frozen OpenAI tool list. Callers reinstall hooks after
    /// filtering so bash and write-review still match what remains.
    pub fn retain_named(&mut self, names: &[String]) {
        self.tools.retain(|t| names.iter().any(|n| n == t.name()));
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
        let result = self.execute_inner(call, ctx).await;
        for hook in &self.hooks {
            hook.after(call, &result, ctx).await;
        }
        result
    }

    async fn execute_inner(&self, call: &ToolCall, ctx: &ToolCtx) -> ToolResult {
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

        for hook in &self.hooks {
            match hook.before(call, &args, ctx).await {
                HookDecision::Allow => {}
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

        // After hooks have had their say: a Deny already returned. Absence of a
        // hook that speaks for bash is still deny. Write-review and similar
        // hooks must not accidentally open the shell. `cd ..` out of the
        // project remains possible — that is why the default is closed.
        if call.name == "bash" && !self.bash_is_governed() {
            return ToolResult::err(
                call,
                "`bash` was not run: no shell policy is installed. A subprocess is not confined \
                 by the workspace capability; install a shell-approval (or allow-bash) hook \
                 before this tool can run.",
            );
        }

        let output = tool.run(&args, ctx).await;
        ToolResult {
            tool_call_id: call.id.clone(),
            name: call.name.clone(),
            content: output.content,
            is_error: output.is_error,
        }
    }

    fn bash_is_governed(&self) -> bool {
        self.hooks
            .iter()
            .any(|h| matches!(h.name(), "shell-approval" | "allow-bash"))
    }
}

impl Default for Registry {
    fn default() -> Self {
        Registry::core()
    }
}

/// For tests and non-UI consumers that have already decided bash may run.
///
/// Named so [`Registry::execute`] treats it as a shell policy. The app's
/// `ShellApprovalHook` is the production equivalent.
pub struct AllowBash;

#[async_trait]
impl ToolHook for AllowBash {
    fn name(&self) -> &'static str {
        "allow-bash"
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
    fn retain_named_drops_everything_not_on_the_list() {
        let mut reg = Registry::core();
        reg.retain_named(&["read".into(), "grep".into()]);
        assert_eq!(reg.names(), vec!["read", "grep"]);
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

    /// An open hold must count through the Instant the budget tick uses, not
    /// through a second `now()` that would miss the wait.
    #[test]
    fn an_open_hold_counts_through_the_tick_instant() {
        let gate = GatePause::default();
        let t0 = Instant::now();
        let _hold = gate.hold();
        let later = t0 + Duration::from_secs(30);
        assert!(
            gate.paused_at(later) >= Duration::from_secs(29),
            "the wait the user spent reading has to be visible to the clock"
        );
    }

    #[test]
    fn dropping_the_hold_stops_the_pause_from_growing() {
        let gate = GatePause::default();
        let t0 = Instant::now();
        {
            let _hold = gate.hold();
        }
        let after = gate.paused_at(t0 + Duration::from_secs(1));
        let later = gate.paused_at(t0 + Duration::from_secs(60));
        assert_eq!(after, later, "released pause must not keep accumulating");
    }

    #[tokio::test]
    async fn core_registry_refuses_bash_until_a_policy_is_installed() {
        let (_t, ctx) = ctx();
        let result = Registry::core()
            .execute(
                &ToolCall::new("1", "bash", r#"{"command":"echo hi"}"#),
                &ctx,
            )
            .await;
        assert!(result.is_error);
        assert!(
            result.content.contains("no shell policy"),
            "{}",
            result.content
        );
    }

    #[tokio::test]
    async fn an_allow_bash_hook_lets_the_command_run() {
        let (_t, ctx) = ctx();
        let result = Registry::core()
            .with_hook(AllowBash)
            .execute(
                &ToolCall::new("1", "bash", r#"{"command":"echo hi"}"#),
                &ctx,
            )
            .await;
        assert!(!result.is_error, "{}", result.content);
        assert_eq!(result.content, "hi");
    }
}
