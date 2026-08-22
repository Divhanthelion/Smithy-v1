//! HTTP and stdio MCP clients via `rmcp`.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use http::{HeaderName, HeaderValue};
use rmcp::model::{CallToolRequestParams, CallToolResult};
use rmcp::service::RunningService;
use rmcp::transport::child_process::TokioChildProcess;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::{RoleClient, ServiceExt};
use serde_json::Value;
use tokio::process::Command;

use super::wrap::McpInvoke;
use super::{
    resolve_placeholder, resolve_secret, ConnectedServer, ListedTool, McpConnector, McpServer,
    McpTransportKind,
};

/// Production connector. Tests inject a fake instead.
pub struct RmcpConnector;

#[async_trait]
impl McpConnector for RmcpConnector {
    async fn connect(
        &self,
        name: &str,
        server: &McpServer,
        project_root: &Path,
    ) -> Result<ConnectedServer, String> {
        match server.kind() {
            Some(McpTransportKind::Http) => connect_http(name, server).await,
            Some(McpTransportKind::Stdio) => connect_stdio(name, server, project_root).await,
            None => Err("no transport".into()),
        }
    }
}

struct RmcpInvoke {
    client: Arc<RunningService<RoleClient, ()>>,
}

#[async_trait]
impl McpInvoke for RmcpInvoke {
    async fn call(&self, remote_name: &str, args: Value) -> Result<String, String> {
        let arguments = match args {
            Value::Object(map) => Some(map),
            Value::Null => None,
            other => {
                return Err(format!(
                    "MCP tool `{remote_name}` arguments must be an object, got {other}"
                ))
            }
        };
        let mut params = CallToolRequestParams::new(remote_name.to_string());
        if let Some(map) = arguments {
            params = params.with_arguments(map);
        }
        let result = self
            .client
            .call_tool(params)
            .await
            .map_err(|e| format!("MCP `{remote_name}` failed: {e}"))?;
        render_result(remote_name, result)
    }
}

async fn connect_http(name: &str, server: &McpServer) -> Result<ConnectedServer, String> {
    let url = server
        .url
        .as_deref()
        .ok_or_else(|| format!("MCP `{name}` is http but has no `url`"))?;
    let mut config = StreamableHttpClientTransportConfig::with_uri(url.to_string());
    if let Some(headers) = &server.headers {
        config.custom_headers = resolved_headers(headers)?;
    }
    let transport = StreamableHttpClientTransport::from_config(config);
    finish_connect(().serve(transport).await.map_err(|e| e.to_string())?).await
}

async fn connect_stdio(
    name: &str,
    server: &McpServer,
    project_root: &Path,
) -> Result<ConnectedServer, String> {
    let command = server
        .command
        .as_deref()
        .ok_or_else(|| format!("MCP `{name}` is stdio but has no `command`"))?;
    let mut cmd = Command::new(command);
    cmd.args(&server.args);
    cmd.env_clear();
    if let Ok(path) = std::env::var("PATH") {
        cmd.env("PATH", path);
    }
    if let Ok(home) = std::env::var("HOME") {
        cmd.env("HOME", home);
    }
    for key in &server.env {
        let value = resolve_secret(key).ok_or_else(|| {
            format!("MCP `{name}` secret `{key}` is not in the keychain or the environment")
        })?;
        cmd.env(key, value);
    }
    cmd.current_dir(project_root);
    cmd.stdin(std::process::Stdio::piped());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    let transport = TokioChildProcess::new(cmd).map_err(|e| e.to_string())?;
    finish_connect(().serve(transport).await.map_err(|e| e.to_string())?).await
}

async fn finish_connect(client: RunningService<RoleClient, ()>) -> Result<ConnectedServer, String> {
    let listed = client
        .list_tools(Default::default())
        .await
        .map_err(|e| format!("tools/list failed: {e}"))?;
    let tools = listed
        .tools
        .into_iter()
        .map(|t| {
            // Per MCP spec, `read_only_hint` defaults to false (the pessimistic assumption).
            let read_only = t
                .annotations
                .as_ref()
                .and_then(|a| a.read_only_hint)
                .unwrap_or(false);
            ListedTool {
                name: t.name.into_owned(),
                description: t.description.map(|d| d.into_owned()).unwrap_or_default(),
                input_schema: Value::Object((*t.input_schema).clone()),
                read_only,
            }
        })
        .collect();
    Ok(ConnectedServer {
        tools,
        invoke: Arc::new(RmcpInvoke {
            client: Arc::new(client),
        }),
    })
}

fn resolved_headers(
    headers: &std::collections::BTreeMap<String, String>,
) -> Result<HashMap<HeaderName, HeaderValue>, String> {
    let mut out = HashMap::new();
    for (key, value) in headers {
        let resolved = resolve_placeholder(value)?;
        let name = HeaderName::from_bytes(key.as_bytes())
            .map_err(|e| format!("MCP header `{key}`: {e}"))?;
        let val = HeaderValue::from_str(&resolved)
            .map_err(|e| format!("MCP header `{key}` value: {e}"))?;
        out.insert(name, val);
    }
    Ok(out)
}

fn render_result(remote_name: &str, result: CallToolResult) -> Result<String, String> {
    let mut parts = Vec::new();
    for block in &result.content {
        if let Some(text) = block.as_text() {
            parts.push(text.text.clone());
        }
    }
    if let Some(structured) = result.structured_content {
        if !structured.is_null() {
            parts.push(structured.to_string());
        }
    }
    let text = if parts.is_empty() {
        String::new()
    } else {
        parts.join("\n")
    };
    if result.is_error.unwrap_or(false) {
        Err(if text.is_empty() {
            format!("MCP `{remote_name}` returned an error")
        } else {
            text
        })
    } else {
        Ok(text)
    }
}
