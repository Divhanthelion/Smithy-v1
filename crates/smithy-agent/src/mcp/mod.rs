//! MCP servers wrapped as Smithy Tools.
//!
//! Not a Session kind, Skill, or Command. Enabled servers in `mcp.json` are
//! connected when a Session is built; their tools are frozen into the prefix
//! like the core set. Smithy still dispatches. Explore does not inherit them.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use smithy_tools::{Registry, Tool, ToolDefinition};

mod client;
mod hook;
mod schema;
mod wrap;

pub use client::RmcpConnector;
pub use hook::McpReviewHook;
pub use wrap::{is_mcp_tool_name, McpInvoke, McpTool, UnavailableMcpTool};

pub(crate) const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
pub(crate) const CALL_TIMEOUT: Duration = Duration::from_secs(60);

const GITHUB_PAT_ENV: &str = "GITHUB_PERSONAL_ACCESS_TOKEN";

/// File shape: object keyed by server name.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct McpFile {
    #[serde(default)]
    pub servers: BTreeMap<String, McpServer>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpServer {
    /// Omitted means off.
    #[serde(default)]
    pub enabled: bool,
    #[serde(rename = "type")]
    pub transport: Option<McpTransportKind>,
    pub url: Option<String>,
    pub headers: Option<BTreeMap<String, String>>,
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    /// Env var names to pass into a stdio child (values from keychain, then env).
    #[serde(default)]
    pub env: Vec<String>,
    /// `None` = every tool `list_tools` returned. `Some([])` = advertise none.
    /// Names are the server's, before `{server}_` prefixing.
    pub allowed_tools: Option<Vec<String>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum McpTransportKind {
    Http,
    Stdio,
}

impl McpServer {
    pub fn kind(&self) -> Option<McpTransportKind> {
        if let Some(k) = self.transport {
            return Some(k);
        }
        if self.url.is_some() {
            return Some(McpTransportKind::Http);
        }
        if self.command.is_some() {
            return Some(McpTransportKind::Stdio);
        }
        None
    }
}

#[derive(Clone)]
pub struct ListedTool {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    /// `true` when the MCP server declared `annotations.readOnlyHint = true`.
    /// `false` (the default) means the tool may mutate remote state.
    pub read_only: bool,
}

pub struct ConnectedServer {
    pub tools: Vec<ListedTool>,
    pub invoke: std::sync::Arc<dyn McpInvoke>,
}

#[async_trait]
pub trait McpConnector: Send + Sync {
    async fn connect(
        &self,
        name: &str,
        server: &McpServer,
        project_root: &Path,
    ) -> Result<ConnectedServer, String>;
}

pub struct McpAttach {
    pub tools: Vec<Box<dyn Tool>>,
    pub notices: Vec<String>,
    /// Names of MCP tools that declared `readOnlyHint = true`.
    pub read_only_tools: Vec<String>,
}

/// Project `.smithy/mcp.json` replaces a user entry of the same name.
pub fn load_mcp_files(project: &Path) -> (McpFile, Vec<String>) {
    let mut notices = Vec::new();
    let mut file = McpFile::default();
    if let Some(home) = std::env::var_os("HOME") {
        overlay(
            &mut file,
            &PathBuf::from(home).join(".smithy/mcp.json"),
            &mut notices,
        );
    }
    overlay(&mut file, &project.join(".smithy/mcp.json"), &mut notices);
    (file, notices)
}

fn overlay(into: &mut McpFile, path: &Path, notices: &mut Vec<String>) {
    let Ok(text) = std::fs::read_to_string(path) else {
        return;
    };
    match serde_json::from_str::<McpFile>(&text) {
        Ok(parsed) => {
            for (name, server) in parsed.servers {
                into.servers.insert(name, server);
            }
        }
        Err(e) => notices.push(format!(
            "MCP config `{}` was not readable ({e}); that file was ignored.",
            path.display()
        )),
    }
}

/// `${NAME}` → keychain (if the sidecar says it is stored) then env. GitHub's PAT uses the `github-pat` account.
pub fn resolve_placeholder(raw: &str) -> Result<String, String> {
    let mut out = String::new();
    let mut rest = raw;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        rest = &rest[start + 2..];
        let Some(end) = rest.find('}') else {
            return Err("unclosed `${` in an MCP header".into());
        };
        let name = &rest[..end];
        rest = &rest[end + 1..];
        let value = resolve_secret(name).ok_or_else(|| {
            format!("MCP secret `{name}` is not in the keychain or the environment")
        })?;
        out.push_str(&value);
    }
    out.push_str(rest);
    Ok(out)
}

pub fn resolve_secret(name: &str) -> Option<String> {
    let account = if name == GITHUB_PAT_ENV {
        crate::config::GITHUB_PAT
    } else {
        name
    };
    crate::config::api_key(account, name)
}

pub fn advertised_name(server: &str, tool: &str) -> String {
    format!("{server}_{tool}")
}

fn is_allowed(server: &McpServer, remote_name: &str) -> bool {
    match &server.allowed_tools {
        None => true,
        Some(list) => list.iter().any(|n| n == remote_name),
    }
}

/// Connect enabled servers and wrap their tools. Failed servers become Notices.
pub async fn attach_mcp(project: &Path, connector: &dyn McpConnector) -> McpAttach {
    let (file, mut notices) = load_mcp_files(project);
    let mut tools: Vec<Box<dyn Tool>> = Vec::new();
    let mut read_only_tools: Vec<String> = Vec::new();
    for (name, server) in &file.servers {
        if !server.enabled {
            continue;
        }
        if matches!(&server.allowed_tools, Some(list) if list.is_empty()) {
            continue;
        }
        if server.kind().is_none() {
            notices.push(format!(
                "MCP `{name}` omitted: set `type` to `http` or `stdio` (or provide `url` / `command`)."
            ));
            continue;
        }
        match tokio::time::timeout(CONNECT_TIMEOUT, connector.connect(name, server, project)).await
        {
            Ok(Ok(connected)) => {
                wrap_listed(
                    name,
                    server,
                    connected,
                    &mut tools,
                    &mut notices,
                    &mut read_only_tools,
                );
            }
            Ok(Err(e)) => notices.push(format!("MCP `{name}` omitted: {e}")),
            Err(_) => notices.push(format!(
                "MCP `{name}` omitted: connect timed out after {}s.",
                CONNECT_TIMEOUT.as_secs()
            )),
        }
    }
    McpAttach {
        tools,
        notices,
        read_only_tools,
    }
}

fn wrap_listed(
    server: &str,
    cfg: &McpServer,
    connected: ConnectedServer,
    tools: &mut Vec<Box<dyn Tool>>,
    notices: &mut Vec<String>,
    read_only_tools: &mut Vec<String>,
) {
    for listed in connected.tools {
        if !is_allowed(cfg, &listed.name) {
            continue;
        }
        let params = match schema::json_schema_to_parameters(&listed.input_schema) {
            Ok(p) => p,
            Err(e) => {
                notices.push(format!(
                    "MCP `{server}` tool `{}` skipped: {e}.",
                    listed.name
                ));
                continue;
            }
        };
        let advertised = advertised_name(server, &listed.name);
        if tools.iter().any(|t| t.name() == advertised) {
            notices.push(format!(
                "MCP `{server}` tool `{}` skipped: name `{advertised}` already taken.",
                listed.name
            ));
            continue;
        }
        let description = if listed.description.is_empty() {
            format!("MCP tool `{advertised}` from server `{server}`.")
        } else {
            listed.description
        };
        if listed.read_only {
            read_only_tools.push(advertised.clone());
        }
        tools.push(Box::new(McpTool {
            name: advertised.clone(),
            remote_name: listed.name,
            definition: ToolDefinition::new(advertised, description, params),
            invoke: connected.invoke.clone(),
            read_only: listed.read_only,
        }));
    }
}

/// Names in the frozen schema that the live registry does not have become
/// stubs, so execute explains the outage instead of "unknown tool".
pub fn stub_unavailable(registry: &mut Registry, stored_tools: &Value) {
    let have: Vec<String> = registry.names().into_iter().map(str::to_string).collect();
    let Some(arr) = stored_tools.as_array() else {
        return;
    };
    for entry in arr {
        let Some(name) = entry.pointer("/function/name").and_then(Value::as_str) else {
            continue;
        };
        if have.iter().any(|h| h == name) {
            continue;
        }
        registry.push(Box::new(UnavailableMcpTool {
            name: name.to_string(),
        }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    struct FailConnect(&'static str);

    #[async_trait]
    impl McpConnector for FailConnect {
        async fn connect(
            &self,
            _name: &str,
            _server: &McpServer,
            _project_root: &Path,
        ) -> Result<ConnectedServer, String> {
            Err(self.0.into())
        }
    }

    struct FakeConnect {
        tools: Vec<ListedTool>,
    }

    struct OkInvoke;

    #[async_trait]
    impl McpInvoke for OkInvoke {
        async fn call(&self, remote_name: &str, _args: Value) -> Result<String, String> {
            Ok(format!("called {remote_name}"))
        }
    }

    #[async_trait]
    impl McpConnector for FakeConnect {
        async fn connect(
            &self,
            _name: &str,
            _server: &McpServer,
            _project_root: &Path,
        ) -> Result<ConnectedServer, String> {
            Ok(ConnectedServer {
                tools: self.tools.clone(),
                invoke: Arc::new(OkInvoke),
            })
        }
    }

    fn write_mcp(dir: &Path, body: &str) {
        std::fs::create_dir_all(dir.join(".smithy")).unwrap();
        std::fs::write(dir.join(".smithy/mcp.json"), body).unwrap();
    }

    #[test]
    fn omitted_enabled_is_off() {
        let raw = r#"{"servers":{"github":{"type":"http","url":"https://example"}}}"#;
        let file: McpFile = serde_json::from_str(raw).unwrap();
        assert!(!file.servers["github"].enabled);
    }

    #[test]
    fn project_replaces_the_whole_user_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let project = tmp.path().join("proj");
        std::fs::create_dir_all(home.join(".smithy")).unwrap();
        std::fs::write(
            home.join(".smithy/mcp.json"),
            r#"{"servers":{"github":{"enabled":true,"type":"http","url":"https://user.example","allowed_tools":["get_me"]}}}"#,
        )
        .unwrap();
        write_mcp(
            &project,
            r#"{"servers":{"github":{"enabled":true,"type":"http","url":"https://project.example"}}}"#,
        );
        let old = std::env::var_os("HOME");
        std::env::set_var("HOME", &home);
        let (file, notices) = load_mcp_files(&project);
        match old {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
        assert!(notices.is_empty(), "{notices:?}");
        let gh = &file.servers["github"];
        assert_eq!(gh.url.as_deref(), Some("https://project.example"));
        assert!(gh.allowed_tools.is_none());
    }

    #[test]
    fn allowed_tools_empty_means_none() {
        let server = McpServer {
            enabled: true,
            transport: Some(McpTransportKind::Http),
            url: Some("https://x".into()),
            headers: None,
            command: None,
            args: Vec::new(),
            env: Vec::new(),
            allowed_tools: Some(vec![]),
        };
        assert!(!is_allowed(&server, "get_me"));
        let all = McpServer {
            allowed_tools: None,
            ..server.clone()
        };
        assert!(is_allowed(&all, "get_me"));
        let some = McpServer {
            allowed_tools: Some(vec!["get_me".into()]),
            ..server
        };
        assert!(is_allowed(&some, "get_me"));
        assert!(!is_allowed(&some, "other"));
    }

    #[test]
    fn names_are_prefixed_before_the_model_sees_them() {
        assert_eq!(
            advertised_name("github", "get_file_contents"),
            "github_get_file_contents"
        );
    }

    #[test]
    fn placeholder_without_a_secret_fails_closed() {
        let err = resolve_placeholder("Bearer ${DOES_NOT_EXIST_SMITHY_MCP}").unwrap_err();
        assert!(err.contains("DOES_NOT_EXIST_SMITHY_MCP"), "{err}");
    }

    #[test]
    fn placeholder_without_braces_is_left_alone() {
        assert_eq!(resolve_placeholder("true").unwrap(), "true");
    }

    #[tokio::test]
    async fn a_failed_connect_omits_tools_and_notices() {
        let tmp = tempfile::tempdir().unwrap();
        write_mcp(
            tmp.path(),
            r#"{"servers":{"github":{"enabled":true,"type":"http","url":"https://example"}}}"#,
        );
        let attach = attach_mcp(tmp.path(), &FailConnect("connection refused")).await;
        assert!(attach.tools.is_empty());
        assert!(
            attach
                .notices
                .iter()
                .any(|n| n.contains("github") && n.contains("connection refused")),
            "{:?}",
            attach.notices
        );
    }

    #[tokio::test]
    async fn allowed_tools_filters_before_prefixing() {
        let tmp = tempfile::tempdir().unwrap();
        write_mcp(
            tmp.path(),
            r#"{"servers":{"github":{"enabled":true,"type":"http","url":"https://example","allowed_tools":["get_me"]}}}"#,
        );
        let connector = FakeConnect {
            tools: vec![
                ListedTool {
                    name: "get_me".into(),
                    description: "whoami".into(),
                    input_schema: serde_json::json!({"type": "object"}),
                    read_only: true,
                },
                ListedTool {
                    name: "create_issue".into(),
                    description: "write".into(),
                    input_schema: serde_json::json!({"type": "object"}),
                    read_only: false,
                },
            ],
        };
        let attach = attach_mcp(tmp.path(), &connector).await;
        let names: Vec<_> = attach.tools.iter().map(|t| t.name().to_string()).collect();
        assert_eq!(names, vec!["github_get_me".to_string()]);
        assert!(attach.notices.is_empty(), "{:?}", attach.notices);
        let openai = attach.tools[0].definition().to_openai();
        assert_eq!(openai["function"]["name"], "github_get_me");
    }

    #[tokio::test]
    async fn disabled_servers_are_not_contacted() {
        let tmp = tempfile::tempdir().unwrap();
        write_mcp(
            tmp.path(),
            r#"{"servers":{"github":{"enabled":false,"type":"http","url":"https://example"}}}"#,
        );
        let attach = attach_mcp(tmp.path(), &FailConnect("should not run")).await;
        assert!(attach.tools.is_empty());
        assert!(attach.notices.is_empty(), "{:?}", attach.notices);
    }

    #[tokio::test]
    async fn unmappable_schema_is_skipped_not_a_hard_fail() {
        let tmp = tempfile::tempdir().unwrap();
        write_mcp(
            tmp.path(),
            r#"{"servers":{"github":{"enabled":true,"type":"http","url":"https://example"}}}"#,
        );
        let connector = FakeConnect {
            tools: vec![ListedTool {
                name: "weird".into(),
                description: String::new(),
                input_schema: serde_json::json!({"$ref": "#/nope"}),
                read_only: false,
            }],
        };
        let attach = attach_mcp(tmp.path(), &connector).await;
        assert!(attach.tools.is_empty());
        assert!(
            attach
                .notices
                .iter()
                .any(|n| n.contains("weird") && n.contains("$ref")),
            "{:?}",
            attach.notices
        );
    }

    #[test]
    fn stub_fills_names_the_live_registry_lost() {
        let mut registry = Registry::new().with(smithy_tools::tools::read::Read);
        let stored = serde_json::json!([
            {"type":"function","function":{"name":"read","description":"","parameters":{"type":"object","properties":{},"required":[]}}},
            {"type":"function","function":{"name":"github_get_me","description":"","parameters":{"type":"object","properties":{},"required":[]}}}
        ]);
        stub_unavailable(&mut registry, &stored);
        let names = registry.names();
        assert!(names.contains(&"read"));
        assert!(names.contains(&"github_get_me"));
    }
}
