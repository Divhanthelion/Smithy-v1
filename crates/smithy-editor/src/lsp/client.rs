//! LSP client implementation
//!
//! This module provides the main `LspClient` struct for communicating
//! with a single language server instance.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use lsp_types::*;
use serde::{de::DeserializeOwned, Serialize};
use serde_json::{json, Value};
use std::str::FromStr;
use thiserror::Error;
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio::time::timeout;

use super::transport::LspTransport;
use super::types::{LspDiagnostic, LspHover, PositionEncoding};

/// Convert a file path to a URI string
fn path_to_uri(path: &Path) -> Result<Uri, LspError> {
    // Percent-encoded by `super::uri`. Building the string inline here is what
    // put a literal space in the URI for any project whose path had one, which
    // the parser rejected and which stopped the server from ever starting.
    let uri_string = super::uri::path_to_uri(path);

    Uri::from_str(&uri_string)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e.to_string()).into())
}

/// Parse a URI string
fn parse_uri(uri: &str) -> Result<Uri, LspError> {
    Uri::from_str(uri)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e.to_string()).into())
}

/// Errors that can occur during LSP operations
#[derive(Debug, Error)]
pub enum LspError {
    #[error("Failed to spawn language server: {0}")]
    SpawnFailed(#[from] std::io::Error),

    #[error("Transport error: {0}")]
    Transport(#[from] super::transport::TransportError),

    #[error("JSON serialization error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("Request timed out after {0:?}")]
    Timeout(Duration),

    #[error("Request failed: {code} - {message}")]
    RequestFailed { code: i64, message: String },

    #[error("Server not initialized")]
    NotInitialized,

    #[error("Channel closed")]
    ChannelClosed,

    #[error("Unexpected response type")]
    UnexpectedResponse,
}

/// Configuration for an LSP client
#[derive(Debug, Clone)]
pub struct LspClientConfig {
    /// Command to run the language server
    pub command: String,
    /// Arguments for the language server command
    pub args: Vec<String>,
    /// Working directory for the server
    pub root_path: PathBuf,
    /// Timeout for requests (default: 30 seconds)
    pub request_timeout: Duration,
    /// Language ID (e.g., "rust", "python")
    pub language_id: String,
    /// Server-specific settings, sent verbatim as `initializationOptions`.
    ///
    /// Sending nothing means the server runs every one of its own defaults,
    /// which for rust-analyzer is expensive — see
    /// `super::registry::rust_initialization_options`.
    pub initialization_options: Option<serde_json::Value>,
}

/// Pending request tracker
struct PendingRequest {
    response_tx: oneshot::Sender<Result<Value, LspError>>,
}

/// Server health status
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServerHealth {
    /// Server is running normally
    Healthy,
    /// Server has crashed and needs restart
    Crashed,
    /// Server is being shut down
    ShuttingDown,
}

/// LSP client for communicating with a language server
pub struct LspClient {
    /// Unique client ID
    id: u64,
    /// Configuration
    config: LspClientConfig,
    /// Child process handle
    _child: Child,
    /// Transport layer
    transport: LspTransport,
    /// Counter for request IDs
    request_id: AtomicU64,
    /// Pending requests awaiting responses
    pending_requests: Arc<Mutex<HashMap<u64, PendingRequest>>>,
    /// Server capabilities (set after initialization)
    capabilities: Arc<Mutex<Option<ServerCapabilities>>>,
    /// Position encoding negotiated with server
    position_encoding: PositionEncoding,
    /// Whether the server has been initialized
    initialized: Arc<Mutex<bool>>,
    /// Channel for diagnostics notifications
    diagnostics_tx: mpsc::Sender<(String, Vec<LspDiagnostic>)>,
    /// Health status of the server
    health: Arc<Mutex<ServerHealth>>,
    /// Crash notification channel
    crash_tx: mpsc::Sender<u64>,
}

impl LspClient {
    /// Spawn a new language server and create a client
    pub async fn spawn(
        id: u64,
        config: LspClientConfig,
        diagnostics_tx: mpsc::Sender<(String, Vec<LspDiagnostic>)>,
        crash_tx: mpsc::Sender<u64>,
    ) -> Result<Self, LspError> {
        let mut child = Command::new(&config.command)
            .args(&config.args)
            .current_dir(&config.root_path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;

        let stdin = child.stdin.take().expect("Failed to open stdin");
        let stdout = child.stdout.take().expect("Failed to open stdout");

        let (transport, incoming_rx) = LspTransport::new(stdin, stdout);
        let pending_requests = Arc::new(Mutex::new(HashMap::new()));
        let health = Arc::new(Mutex::new(ServerHealth::Healthy));

        let client = Self {
            id,
            config,
            _child: child,
            transport,
            request_id: AtomicU64::new(1),
            pending_requests: pending_requests.clone(),
            capabilities: Arc::new(Mutex::new(None)),
            position_encoding: PositionEncoding::Utf16,
            initialized: Arc::new(Mutex::new(false)),
            diagnostics_tx,
            health,
            crash_tx,
        };

        // Spawn message handler
        client.spawn_message_handler(incoming_rx);

        Ok(client)
    }

    /// Get the current health status of the server
    pub async fn health(&self) -> ServerHealth {
        *self.health.lock().await
    }

    /// Get the client ID
    pub fn id(&self) -> u64 {
        self.id
    }

    /// Get the language ID
    pub fn language_id(&self) -> &str {
        &self.config.language_id
    }

    /// Get the root path
    pub fn root_path(&self) -> &Path {
        &self.config.root_path
    }

    /// Get the position encoding
    pub fn position_encoding(&self) -> PositionEncoding {
        self.position_encoding
    }

    /// Spawn message handler task
    fn spawn_message_handler(&self, mut rx: mpsc::Receiver<String>) {
        let pending = self.pending_requests.clone();
        let diagnostics_tx = self.diagnostics_tx.clone();
        let client_id = self.id;
        let health = self.health.clone();
        let crash_tx = self.crash_tx.clone();

        tokio::spawn(async move {
            while let Some(message) = rx.recv().await {
                if let Err(e) = Self::handle_message(&pending, &diagnostics_tx, &message).await {
                    eprintln!("LSP message handler error: {}", e);
                }
            }

            // Channel closed - server likely crashed
            let mut health_guard = health.lock().await;
            if *health_guard == ServerHealth::Healthy {
                eprintln!(
                    "LSP server {} disconnected unexpectedly (crashed?)",
                    client_id
                );
                *health_guard = ServerHealth::Crashed;
                drop(health_guard);

                // Notify about the crash
                let _ = crash_tx.send(client_id).await;
            }
        });
    }

    /// Handle an incoming message from the server
    async fn handle_message(
        pending: &Arc<Mutex<HashMap<u64, PendingRequest>>>,
        diagnostics_tx: &mpsc::Sender<(String, Vec<LspDiagnostic>)>,
        message: &str,
    ) -> Result<(), LspError> {
        let value: Value = serde_json::from_str(message)?;

        // Check if it's a response (has "id" and "result" or "error")
        if let Some(id) = value.get("id").and_then(|v| v.as_u64()) {
            let mut pending_guard = pending.lock().await;
            if let Some(request) = pending_guard.remove(&id) {
                let result = if let Some(error) = value.get("error") {
                    let code = error.get("code").and_then(|c| c.as_i64()).unwrap_or(0);
                    let message = error
                        .get("message")
                        .and_then(|m| m.as_str())
                        .unwrap_or("Unknown error")
                        .to_string();
                    Err(LspError::RequestFailed { code, message })
                } else if let Some(result) = value.get("result") {
                    Ok(result.clone())
                } else {
                    Ok(Value::Null)
                };
                let _ = request.response_tx.send(result);
            }
        }
        // Check if it's a notification (has "method" but no "id")
        else if let Some(method) = value.get("method").and_then(|m| m.as_str()) {
            match method {
                "textDocument/publishDiagnostics" => {
                    if let Some(params) = value.get("params") {
                        if let Ok(diag_params) =
                            serde_json::from_value::<PublishDiagnosticsParams>(params.clone())
                        {
                            let uri = diag_params.uri.to_string();
                            let diagnostics: Vec<LspDiagnostic> = diag_params
                                .diagnostics
                                .into_iter()
                                .map(LspDiagnostic::from)
                                .collect();
                            let _ = diagnostics_tx.send((uri, diagnostics)).await;
                        }
                    }
                }
                "window/logMessage" | "window/showMessage" => {
                    // Log server messages
                    if let Some(params) = value.get("params") {
                        if let Some(message) = params.get("message").and_then(|m| m.as_str()) {
                            eprintln!("LSP: {}", message);
                        }
                    }
                }
                _ => {
                    // Unknown notification
                }
            }
        }

        Ok(())
    }

    /// Send a request and wait for response
    async fn request<P: Serialize, R: DeserializeOwned>(
        &self,
        method: &str,
        params: P,
    ) -> Result<R, LspError> {
        let id = self.request_id.fetch_add(1, Ordering::SeqCst);

        let request = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });

        let message = serde_json::to_string(&request)?;

        // Set up response channel
        let (response_tx, response_rx) = oneshot::channel();
        {
            let mut pending = self.pending_requests.lock().await;
            pending.insert(id, PendingRequest { response_tx });
        }

        // Send request
        self.transport.send(message).await?;

        // Wait for response with timeout
        let result = timeout(self.config.request_timeout, response_rx)
            .await
            .map_err(|_| LspError::Timeout(self.config.request_timeout))?
            .map_err(|_| LspError::ChannelClosed)??;

        // Deserialize response
        serde_json::from_value(result).map_err(LspError::from)
    }

    /// Send a notification (no response expected)
    async fn notify<P: Serialize>(&self, method: &str, params: P) -> Result<(), LspError> {
        let notification = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });

        let message = serde_json::to_string(&notification)?;
        self.transport.send(message).await?;
        Ok(())
    }

    /// Initialize the language server
    pub async fn initialize(&self) -> Result<(), LspError> {
        let root_uri = path_to_uri(&self.config.root_path)?;

        // `root_path` and `root_uri` are deprecated in favour of
        // `workspace_folders`, which is already set below and carries the same
        // information. Sending all three was redundant.
        let params = InitializeParams {
            process_id: Some(std::process::id()),
            initialization_options: self.config.initialization_options.clone(),

            capabilities: ClientCapabilities {
                text_document: Some(TextDocumentClientCapabilities {
                    hover: Some(HoverClientCapabilities {
                        dynamic_registration: Some(false),
                        content_format: Some(vec![MarkupKind::Markdown, MarkupKind::PlainText]),
                    }),
                    completion: Some(CompletionClientCapabilities {
                        dynamic_registration: Some(false),
                        completion_item: Some(CompletionItemCapability {
                            snippet_support: Some(true),
                            documentation_format: Some(vec![
                                MarkupKind::Markdown,
                                MarkupKind::PlainText,
                            ]),
                            ..Default::default()
                        }),
                        ..Default::default()
                    }),
                    synchronization: Some(TextDocumentSyncClientCapabilities {
                        dynamic_registration: Some(false),
                        will_save: Some(false),
                        will_save_wait_until: Some(false),
                        did_save: Some(true),
                    }),
                    publish_diagnostics: Some(PublishDiagnosticsClientCapabilities {
                        related_information: Some(true),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                workspace: Some(WorkspaceClientCapabilities {
                    workspace_folders: Some(true),
                    ..Default::default()
                }),
                ..Default::default()
            },
            workspace_folders: Some(vec![WorkspaceFolder {
                uri: root_uri,
                name: self
                    .config
                    .root_path
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| "workspace".to_string()),
            }]),
            ..Default::default()
        };

        let result: InitializeResult = self.request("initialize", params).await?;

        // Store capabilities
        {
            let mut caps = self.capabilities.lock().await;
            *caps = Some(result.capabilities);
        }

        // Send initialized notification
        self.notify("initialized", InitializedParams {}).await?;

        // Mark as initialized
        {
            let mut init = self.initialized.lock().await;
            *init = true;
        }

        Ok(())
    }

    /// Notify the server that a document was opened
    pub async fn did_open(&self, uri: &str, language_id: &str, text: &str) -> Result<(), LspError> {
        self.ensure_initialized().await?;

        let params = DidOpenTextDocumentParams {
            text_document: TextDocumentItem {
                uri: parse_uri(uri)?,
                language_id: language_id.to_string(),
                version: 1,
                text: text.to_string(),
            },
        };

        self.notify("textDocument/didOpen", params).await
    }

    /// Notify the server that a document was changed
    pub async fn did_change(&self, uri: &str, version: i32, text: &str) -> Result<(), LspError> {
        self.ensure_initialized().await?;

        let params = DidChangeTextDocumentParams {
            text_document: VersionedTextDocumentIdentifier {
                uri: parse_uri(uri)?,
                version,
            },
            content_changes: vec![TextDocumentContentChangeEvent {
                range: None,
                range_length: None,
                text: text.to_string(),
            }],
        };

        self.notify("textDocument/didChange", params).await
    }

    /// Notify the server that a document was closed
    pub async fn did_close(&self, uri: &str) -> Result<(), LspError> {
        self.ensure_initialized().await?;

        let params = DidCloseTextDocumentParams {
            text_document: TextDocumentIdentifier {
                uri: parse_uri(uri)?,
            },
        };

        self.notify("textDocument/didClose", params).await
    }

    /// Request hover information at a position
    pub async fn hover(
        &self,
        uri: &str,
        line: u32,
        character: u32,
    ) -> Result<Option<LspHover>, LspError> {
        self.ensure_initialized().await?;

        let params = HoverParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier {
                    uri: parse_uri(uri)?,
                },
                position: Position { line, character },
            },
            work_done_progress_params: Default::default(),
        };

        let result: Option<Hover> = self.request("textDocument/hover", params).await?;
        Ok(result.map(LspHover::from))
    }

    /// Request go to definition
    pub async fn goto_definition(
        &self,
        uri: &str,
        line: u32,
        character: u32,
    ) -> Result<Option<Location>, LspError> {
        self.ensure_initialized().await?;

        let params = GotoDefinitionParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier {
                    uri: parse_uri(uri)?,
                },
                position: Position { line, character },
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        };

        let result: Option<GotoDefinitionResponse> =
            self.request("textDocument/definition", params).await?;

        Ok(match result {
            Some(GotoDefinitionResponse::Scalar(loc)) => Some(loc),
            Some(GotoDefinitionResponse::Array(locs)) => locs.into_iter().next(),
            Some(GotoDefinitionResponse::Link(links)) => {
                links.into_iter().next().map(|l| Location {
                    uri: l.target_uri,
                    range: l.target_selection_range,
                })
            }
            None => None,
        })
    }

    /// Shutdown the language server gracefully
    pub async fn shutdown(&self) -> Result<(), LspError> {
        // Mark as shutting down to prevent crash detection
        let mut health = self.health.lock().await;
        *health = ServerHealth::ShuttingDown;
        drop(health);

        // Send shutdown request
        let _: Value = self.request("shutdown", Value::Null).await?;

        // Send exit notification
        self.notify("exit", Value::Null).await?;

        Ok(())
    }

    /// Ensure the server is initialized
    async fn ensure_initialized(&self) -> Result<(), LspError> {
        let initialized = self.initialized.lock().await;
        if !*initialized {
            return Err(LspError::NotInitialized);
        }
        Ok(())
    }
}

impl Drop for LspClient {
    fn drop(&mut self) {
        // Mark as shutting down to prevent false crash detection
        // Note: This is best-effort since we can't await in drop
        // Child process is killed automatically due to kill_on_drop(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The exact failure from the log: opening a project at
    /// `/Users/rj/Desktop/terminal empire` produced
    /// `file:///Users/rj/Desktop/terminal empire`, and `Uri::from_str` rejected
    /// it with "unexpected character at index 33" — index 33 being the space.
    /// The server never started, and the restart loop failed the same way three
    /// times before giving up.
    ///
    /// Asserted here rather than only in `super::uri` because this is the call
    /// that actually failed: the encoding is only correct if the URI parser
    /// accepts the result.
    #[test]
    fn a_project_path_with_a_space_yields_a_uri_the_parser_accepts() {
        let uri = path_to_uri(Path::new("/Users/rj/Desktop/terminal empire"))
            .expect("a space must not make the URI unparseable");

        assert_eq!(uri.as_str(), "file:///Users/rj/Desktop/terminal%20empire");
    }

    /// Every file inside such a project has to work too, not just its root.
    #[test]
    fn a_file_inside_a_spaced_path_yields_a_uri_the_parser_accepts() {
        let uri = path_to_uri(Path::new("/Users/rj/Desktop/terminal empire/src/events.rs"))
            .expect("a space must not make the URI unparseable");

        assert!(uri.as_str().ends_with("/src/events.rs"), "{}", uri.as_str());
        assert!(!uri.as_str().contains(' '));
    }
}
