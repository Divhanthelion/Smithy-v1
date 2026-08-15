//! One Smithy [`Tool`] per advertised MCP tool.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;
use smithy_tools::schema::ToolOutput;
use smithy_tools::{Tool, ToolCtx, ToolDefinition};

use super::CALL_TIMEOUT;

/// `tools/call` against a connected server.
#[async_trait]
pub trait McpInvoke: Send + Sync {
    async fn call(&self, remote_name: &str, args: Value) -> Result<String, String>;
}

pub struct McpTool {
    pub name: String,
    pub remote_name: String,
    pub definition: ToolDefinition,
    pub invoke: Arc<dyn McpInvoke>,
}

#[async_trait]
impl Tool for McpTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn definition(&self) -> ToolDefinition {
        self.definition.clone()
    }

    async fn run(&self, args: &Value, _ctx: &ToolCtx) -> ToolOutput {
        match tokio::time::timeout(
            CALL_TIMEOUT,
            self.invoke.call(&self.remote_name, args.clone()),
        )
        .await
        {
            Ok(Ok(text)) => ToolOutput::ok(text),
            Ok(Err(e)) => ToolOutput::err(e),
            Err(_) => ToolOutput::err(format!(
                "MCP tool `{}` timed out after {}s",
                self.name,
                CALL_TIMEOUT.as_secs()
            )),
        }
    }
}

/// Advertised on resume when the live registry no longer has the name.
pub struct UnavailableMcpTool {
    pub name: String,
}

#[async_trait]
impl Tool for UnavailableMcpTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new(
            self.name.clone(),
            "Unavailable. Listed so the Session prefix does not change.",
            vec![],
        )
    }

    async fn run(&self, _args: &Value, _ctx: &ToolCtx) -> ToolOutput {
        ToolOutput::err(format!(
            "`{}` is advertised in this Session but the server is unavailable. \
             The name stays so the prefix does not change; it cannot run until \
             that server is up and you start a new Session.",
            self.name
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smithy_tools::Workspace;

    struct Boom;

    #[async_trait]
    impl McpInvoke for Boom {
        async fn call(&self, _remote_name: &str, _args: Value) -> Result<String, String> {
            Err("remote said no".into())
        }
    }

    #[tokio::test]
    async fn a_failed_call_is_a_tool_error_not_a_panic() {
        let tool = McpTool {
            name: "github_get_me".into(),
            remote_name: "get_me".into(),
            definition: ToolDefinition::new("github_get_me", "me", vec![]),
            invoke: Arc::new(Boom),
        };
        let dir = tempfile::tempdir().unwrap();
        let ctx = ToolCtx::new(Workspace::open(dir.path()).unwrap());
        let out = tool.run(&Value::Object(Default::default()), &ctx).await;
        assert!(out.is_error);
        assert!(out.content.contains("remote said no"));
    }

    #[tokio::test]
    async fn a_missing_server_explains_why_the_name_still_exists() {
        let tool = UnavailableMcpTool {
            name: "github_get_me".into(),
        };
        let dir = tempfile::tempdir().unwrap();
        let ctx = ToolCtx::new(Workspace::open(dir.path()).unwrap());
        let out = tool.run(&Value::Object(Default::default()), &ctx).await;
        assert!(out.is_error);
        assert!(out.content.contains("unavailable"));
        assert!(out.content.contains("prefix"));
    }
}
