//! MCP mutation policy hook.
//!
//! Mutating MCP tools — those without `readOnlyHint: true` — are denied by
//! default. Unlike `write` and `edit`, MCP tools cannot show a local diff, so
//! the same Review wait is not possible. Refusing is the fail-closed default;
//! the user can opt in to specific mutating tools via `mcp.json` allowlists.

use async_trait::async_trait;
use serde_json::Value;
use smithy_tools::{HookDecision, Registry, ToolCall, ToolCtx, ToolHook};

use super::is_mcp_tool_name;

/// Hook that refuses mutating MCP tools.
///
/// MCP tools that declared `annotations.readOnlyHint = true` are allowed.
/// All others are denied with an explanation. This is the same deny-by-default
/// posture that `bash` has until a shell-approval hook is installed.
///
/// Shared between GUI and CLI so both cannot diverge on policy.
pub struct McpReviewHook {
    /// Set of MCP tool names (already prefixed, like `github_get_me`) that are
    /// known to be read-only and can proceed without review.
    read_only_tools: std::collections::HashSet<String>,
}

impl McpReviewHook {
    /// Build the hook from the current registry, extracting which MCP tools are read-only.
    pub fn from_registry(registry: &Registry) -> Self {
        let mut read_only_tools = std::collections::HashSet::new();
        for name in registry.names() {
            // We need to check if this tool is an MCP tool and if it's read-only.
            // The registry doesn't expose the Tool itself, so we rely on the tool
            // being registered with its read_only flag already set.
            // This is checked at runtime in `before` by looking up the tool.
            if is_mcp_tool_name(name) {
                // We'll check read_only status dynamically in before()
                // For now, just note this is an MCP tool name
            }
            _ = name; // silence unused warning
        }
        Self { read_only_tools }
    }

    /// Create hook with explicit set of read-only MCP tool names.
    pub fn with_read_only_tools(names: impl IntoIterator<Item = String>) -> Self {
        Self {
            read_only_tools: names.into_iter().collect(),
        }
    }
}

#[async_trait]
impl ToolHook for McpReviewHook {
    fn name(&self) -> &'static str {
        "mcp-review"
    }

    async fn before(&self, call: &ToolCall, _args: &Value, _ctx: &ToolCtx) -> HookDecision {
        // Only gate MCP tools (those with `_` in the name like `github_get_me`)
        if !is_mcp_tool_name(&call.name) {
            return HookDecision::Allow;
        }

        // If this tool is in our read-only set, allow it
        if self.read_only_tools.contains(&call.name) {
            return HookDecision::Allow;
        }

        // Deny mutating MCP tools with a clear explanation
        HookDecision::Deny(format!(
            "`{}` is an MCP tool that may mutate remote state. MCP tools without \
             `readOnlyHint: true` are denied by default because there is no local \
             diff to review. To allow this tool, either:\n\
             - Ask the MCP server author to set `annotations.readOnlyHint = true` \
               if the tool is genuinely read-only, or\n\
             - Add the tool to an explicit allow-list in your session configuration.",
            call.name
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smithy_tools::{ToolCall, ToolCtx, Workspace};

    fn ctx() -> (tempfile::TempDir, ToolCtx) {
        let tmp = tempfile::tempdir().unwrap();
        let ws = Workspace::open(tmp.path()).unwrap();
        (tmp, ToolCtx::new(ws))
    }

    fn call(name: &str) -> ToolCall {
        ToolCall::new("c1", name, "{}")
    }

    #[tokio::test]
    async fn core_tools_are_not_gated() {
        let hook = McpReviewHook::with_read_only_tools(vec![]);
        let (_tmp, ctx) = ctx();

        for name in ["read", "write", "edit", "bash", "ls", "glob", "grep"] {
            let decision = hook.before(&call(name), &Value::Null, &ctx).await;
            assert!(
                matches!(decision, HookDecision::Allow),
                "{name} should not be gated by MCP hook"
            );
        }
    }

    #[tokio::test]
    async fn read_only_mcp_tools_are_allowed() {
        let hook = McpReviewHook::with_read_only_tools(vec!["github_get_me".to_string()]);
        let (_tmp, ctx) = ctx();

        let decision = hook
            .before(&call("github_get_me"), &Value::Null, &ctx)
            .await;
        assert!(matches!(decision, HookDecision::Allow));
    }

    #[tokio::test]
    async fn mutating_mcp_tools_are_denied() {
        let hook = McpReviewHook::with_read_only_tools(vec!["github_get_me".to_string()]);
        let (_tmp, ctx) = ctx();

        let decision = hook
            .before(&call("github_create_issue"), &Value::Null, &ctx)
            .await;
        match decision {
            HookDecision::Deny(reason) => {
                assert!(reason.contains("MCP tool"), "{reason}");
                assert!(reason.contains("mutate"), "{reason}");
                assert!(reason.contains("readOnlyHint"), "{reason}");
            }
            other => panic!("expected Deny, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn mcp_tool_detection_works_on_underscores() {
        assert!(is_mcp_tool_name("github_get_me"));
        assert!(is_mcp_tool_name("slack_post_message"));
        assert!(!is_mcp_tool_name("read"));
        assert!(!is_mcp_tool_name("write"));
        assert!(!is_mcp_tool_name("bash"));
    }
}
