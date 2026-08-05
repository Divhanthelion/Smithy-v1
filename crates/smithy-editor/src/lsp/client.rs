//! LSP client implementation
//!
//! This module provides the main `LspClient` struct for communicating
//! with a single language server instance.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use lsp_types::*;
use serde::{de::DeserializeOwned, Serialize};
use serde_json::{json, Value};
use std::str::FromStr;
use thiserror::Error;
use tokio::io::AsyncReadExt;
use tokio::process::{Child, ChildStderr, Command};
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio::time::timeout;

use super::transport::LspTransport;
use super::types::{LspCompletion, LspDiagnostic, LspHover, PositionEncoding};

/// A bounded tail is enough to explain a failed startup without allowing a
/// chatty server to retain memory forever. Before stderr was drained at all,
/// rust-analyzer could fill the OS pipe and block its entire process.
const STDERR_TAIL_BYTES: usize = 16 * 1024;
/// Shutdown cannot inherit the ordinary 30-second interactive request timeout:
/// the window used to remain resident after the user had already quit.
const SHUTDOWN_REQUEST_TIMEOUT: Duration = Duration::from_secs(1);
/// Servers normally reap immediately after `exit`; this grace catches that path
/// without leaving a wedged child behind at application termination.
const SHUTDOWN_EXIT_GRACE: Duration = Duration::from_millis(500);

/// Identity and diagnostics captured when one exact server process disconnects.
#[derive(Debug, Clone)]
pub struct ClientCrash {
    pub client_id: u64,
    pub language_id: String,
    pub root_path: PathBuf,
    pub stderr_tail: String,
}

/// Identity of one exact server process within one registry generation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LspStamp {
    pub root_path: PathBuf,
    pub generation: u64,
    pub client_id: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct ClientDiagnostics {
    pub stamp: LspStamp,
    pub uri: String,
    pub diagnostics: Vec<LspDiagnostic>,
}

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

    #[error("Language server startup failed: {0}")]
    Startup(String),
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

impl LspClientConfig {
    /// Create config for TypeScript language server
    pub fn typescript(root_path: impl Into<PathBuf>) -> Self {
        Self {
            command: "typescript-language-server".to_string(),
            args: vec!["--stdio".to_string()],
            root_path: root_path.into(),
            request_timeout: Duration::from_secs(30),
            language_id: "typescript".to_string(),
            initialization_options: None,
        }
    }

    /// Create config for Python language server (pylsp)
    pub fn python(root_path: impl Into<PathBuf>) -> Self {
        Self {
            command: "pylsp".to_string(),
            args: vec![],
            root_path: root_path.into(),
            request_timeout: Duration::from_secs(30),
            language_id: "python".to_string(),
            initialization_options: None,
        }
    }
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
    child: Mutex<Option<Child>>,
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
    diagnostics_tx: mpsc::Sender<ClientDiagnostics>,
    /// Health status of the server
    health: Arc<Mutex<ServerHealth>>,
    /// Crash notification channel
    crash_tx: mpsc::Sender<ClientCrash>,
    /// Continuously-drained, fixed-size stderr suffix.
    stderr_tail: Arc<StdMutex<VecDeque<u8>>>,
    stamp: LspStamp,
}

impl LspClient {
    /// Spawn a new language server and create a client
    pub async fn spawn(
        id: u64,
        generation: u64,
        config: LspClientConfig,
        diagnostics_tx: mpsc::Sender<ClientDiagnostics>,
        crash_tx: mpsc::Sender<ClientCrash>,
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
        let stderr = child.stderr.take().expect("Failed to open stderr");

        let (transport, incoming_rx) = LspTransport::new(stdin, stdout);
        let pending_requests = Arc::new(Mutex::new(HashMap::new()));
        let health = Arc::new(Mutex::new(ServerHealth::Healthy));
        let stderr_tail = Arc::new(StdMutex::new(VecDeque::with_capacity(
            STDERR_TAIL_BYTES,
        )));
        Self::spawn_stderr_drain(stderr, stderr_tail.clone());

        let stamp = LspStamp {
            root_path: config.root_path.clone(),
            generation,
            client_id: Some(id),
        };
        let client = Self {
            id,
            config,
            child: Mutex::new(Some(child)),
            transport,
            request_id: AtomicU64::new(1),
            pending_requests: pending_requests.clone(),
            capabilities: Arc::new(Mutex::new(None)),
            position_encoding: PositionEncoding::Utf16,
            initialized: Arc::new(Mutex::new(false)),
            diagnostics_tx,
            health,
            crash_tx,
            stderr_tail,
            stamp,
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

    pub fn stamp(&self) -> LspStamp {
        self.stamp.clone()
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

    /// Recent server stderr for startup and crash diagnostics.
    pub fn stderr_tail(&self) -> String {
        Self::stderr_tail_string(&self.stderr_tail)
    }

    /// Spawn message handler task
    fn spawn_message_handler(&self, mut rx: mpsc::Receiver<String>) {
        let pending = self.pending_requests.clone();
        let diagnostics_tx = self.diagnostics_tx.clone();
        let client_id = self.id;
        let health = self.health.clone();
        let crash_tx = self.crash_tx.clone();
        let language_id = self.config.language_id.clone();
        let root_path = self.config.root_path.clone();
        let stderr_tail = self.stderr_tail.clone();
        let stamp = self.stamp.clone();

        tokio::spawn(async move {
            while let Some(message) = rx.recv().await {
                if let Err(e) =
                    Self::handle_message(&pending, &diagnostics_tx, &stamp, &message).await
                {
                    eprintln!("LSP message handler error: {}", e);
                }
            }

            // No response can arrive after stdout closes. Leaving these senders
            // in the map made callers wait until their individual timeouts.
            Self::fail_all_pending(&pending).await;

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
                let tail = Self::stderr_tail_string(&stderr_tail);
                let _ = crash_tx
                    .send(ClientCrash {
                        client_id,
                        language_id,
                        root_path,
                        stderr_tail: tail,
                    })
                    .await;
            }
        });
    }

    fn spawn_stderr_drain(mut stderr: ChildStderr, tail: Arc<StdMutex<VecDeque<u8>>>) {
        tokio::spawn(async move {
            let mut chunk = [0u8; 4096];
            loop {
                match stderr.read(&mut chunk).await {
                    Ok(0) | Err(_) => break,
                    Ok(read) => {
                        let mut tail = tail.lock().unwrap_or_else(|e| e.into_inner());
                        Self::append_stderr(&mut tail, &chunk[..read]);
                    }
                }
            }
        });
    }

    fn append_stderr(tail: &mut VecDeque<u8>, bytes: &[u8]) {
        for byte in bytes {
            if tail.len() == STDERR_TAIL_BYTES {
                tail.pop_front();
            }
            tail.push_back(*byte);
        }
    }

    fn stderr_tail_string(tail: &StdMutex<VecDeque<u8>>) -> String {
        let bytes: Vec<_> = tail
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .copied()
            .collect();
        String::from_utf8_lossy(&bytes).into_owned()
    }

    async fn fail_all_pending(pending: &Arc<Mutex<HashMap<u64, PendingRequest>>>) {
        let requests: Vec<_> = pending.lock().await.drain().map(|(_, p)| p).collect();
        for request in requests {
            let _ = request.response_tx.send(Err(LspError::ChannelClosed));
        }
    }

    /// Handle an incoming message from the server
    async fn handle_message(
        pending: &Arc<Mutex<HashMap<u64, PendingRequest>>>,
        diagnostics_tx: &mpsc::Sender<ClientDiagnostics>,
        stamp: &LspStamp,
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
                            let _ = diagnostics_tx
                                .send(ClientDiagnostics {
                                    stamp: stamp.clone(),
                                    uri,
                                    diagnostics,
                                })
                                .await;
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
        self.request_with_timeout(method, params, self.config.request_timeout)
            .await
    }

    async fn request_with_timeout<P: Serialize, R: DeserializeOwned>(
        &self,
        method: &str,
        params: P,
        request_timeout: Duration,
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
        match timeout(request_timeout, self.transport.send(message)).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                self.pending_requests.lock().await.remove(&id);
                return Err(error.into());
            }
            Err(_) => {
                self.pending_requests.lock().await.remove(&id);
                return Err(LspError::Timeout(request_timeout));
            }
        }

        // Wait for response with timeout
        let result = Self::await_response(
            &self.pending_requests,
            id,
            response_rx,
            request_timeout,
        )
        .await?;

        // Deserialize response
        serde_json::from_value(result).map_err(LspError::from)
    }

    async fn await_response(
        pending: &Arc<Mutex<HashMap<u64, PendingRequest>>>,
        id: u64,
        response_rx: oneshot::Receiver<Result<Value, LspError>>,
        request_timeout: Duration,
    ) -> Result<Value, LspError> {
        match timeout(request_timeout, response_rx).await {
            Ok(Ok(result)) => Ok(result?),
            Ok(Err(_)) => {
                pending.lock().await.remove(&id);
                Err(LspError::ChannelClosed)
            }
            Err(_) => {
                pending.lock().await.remove(&id);
                Err(LspError::Timeout(request_timeout))
            }
        }
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
    pub async fn did_open(
        &self,
        uri: &str,
        language_id: &str,
        version: i32,
        text: &str,
    ) -> Result<(), LspError> {
        self.ensure_initialized().await?;

        let params = DidOpenTextDocumentParams {
            text_document: TextDocumentItem {
                uri: parse_uri(uri)?,
                language_id: language_id.to_string(),
                version,
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

    /// Request completions at a position
    pub async fn completion(
        &self,
        uri: &str,
        line: u32,
        character: u32,
    ) -> Result<Vec<LspCompletion>, LspError> {
        self.ensure_initialized().await?;

        let params = CompletionParams {
            text_document_position: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier {
                    uri: parse_uri(uri)?,
                },
                position: Position { line, character },
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
            context: None,
        };

        let result: Option<CompletionResponse> =
            self.request("textDocument/completion", params).await?;

        let items = match result {
            Some(CompletionResponse::Array(items)) => items,
            Some(CompletionResponse::List(list)) => list.items,
            None => vec![],
        };

        Ok(items.into_iter().map(LspCompletion::from).collect())
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

        // `exit` is attempted even when `shutdown` times out. Several servers
        // stop answering requests while still accepting the final notification.
        let shutdown_result = timeout(
            SHUTDOWN_REQUEST_TIMEOUT,
            self.request_with_timeout::<_, Value>(
                "shutdown",
                Value::Null,
                SHUTDOWN_REQUEST_TIMEOUT,
            ),
        )
        .await
        .map_err(|_| LspError::Timeout(SHUTDOWN_REQUEST_TIMEOUT))
        .and_then(|result| result);
        let exit_result = timeout(
            SHUTDOWN_REQUEST_TIMEOUT,
            self.notify("exit", Value::Null),
        )
        .await
        .map_err(|_| LspError::Timeout(SHUTDOWN_REQUEST_TIMEOUT))
        .and_then(|result| result);
        self.transport.close_writer().await;

        let mut child = self.child.lock().await.take();
        if let Some(mut child) = child.take() {
            if timeout(SHUTDOWN_EXIT_GRACE, child.wait()).await.is_err() {
                let _ = timeout(SHUTDOWN_EXIT_GRACE, child.kill()).await;
                // `kill` requests termination; `wait` is still required to reap
                // the process and avoid zombies during repeated project switches.
                let _ = timeout(SHUTDOWN_EXIT_GRACE, child.wait()).await;
            }
        }

        shutdown_result.and(exit_result)
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

    /// A request that timed out used to leave its sender in the pending map
    /// forever. Repeated hovers then grew the map even though none could ever
    /// receive another response.
    #[tokio::test]
    async fn a_timed_out_request_removes_its_pending_entry() {
        let pending = Arc::new(Mutex::new(HashMap::new()));
        let (tx, rx) = oneshot::channel();
        pending
            .lock()
            .await
            .insert(7, PendingRequest { response_tx: tx });

        let result =
            LspClient::await_response(&pending, 7, rx, Duration::from_millis(1)).await;

        assert!(matches!(result, Err(LspError::Timeout(_))));
        assert!(pending.lock().await.is_empty());
    }

    /// When stdout closes there is no future response to wait for. Previously
    /// every in-flight request sat for its full timeout instead of failing at
    /// the instant the transport ended.
    #[tokio::test]
    async fn transport_closure_fails_every_pending_sender() {
        let pending = Arc::new(Mutex::new(HashMap::new()));
        let (tx, rx) = oneshot::channel();
        pending
            .lock()
            .await
            .insert(9, PendingRequest { response_tx: tx });

        LspClient::fail_all_pending(&pending).await;

        assert!(matches!(rx.await, Ok(Err(LspError::ChannelClosed))));
        assert!(pending.lock().await.is_empty());
    }

    /// rust-analyzer can print indefinitely during a build. The pipe must be
    /// drained, but retaining all of it merely moves the unbounded failure from
    /// the kernel pipe into Smithy's heap.
    #[test]
    fn server_stderr_is_bounded_to_the_most_recent_tail() {
        let mut tail = VecDeque::new();
        let bytes = vec![b'x'; STDERR_TAIL_BYTES + 37];
        LspClient::append_stderr(&mut tail, &bytes);
        assert_eq!(tail.len(), STDERR_TAIL_BYTES);
    }

    /// A server that accepts `initialize` but never answers `shutdown` used to
    /// keep the application alive for the ordinary 30-second request timeout.
    /// Exit must still be attempted, then the child killed and reaped.
    #[cfg(unix)]
    #[tokio::test]
    async fn shutdown_timeout_still_kills_reaps_and_returns_boundedly() {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;

        let mut script = tempfile::NamedTempFile::new().unwrap();
        script
            .write_all(
                br#"#!/bin/sh
read_message() {
  length=
  while IFS= read -r line; do
    line=$(printf '%s' "$line" | tr -d '\r')
    [ -z "$line" ] && break
    case "$line" in Content-Length:*) length=${line#Content-Length: };; esac
  done
  dd bs=1 count="$length" of=/dev/null 2>/dev/null
}
read_message
body='{"jsonrpc":"2.0","id":1,"result":{"capabilities":{}}}'
printf 'Content-Length: %s\r\n\r\n%s' "${#body}" "$body"
read_message
read_message
sleep 10
"#,
            )
            .unwrap();
        let mut permissions = script.as_file().metadata().unwrap().permissions();
        permissions.set_mode(0o755);
        script.as_file().set_permissions(permissions).unwrap();

        let (diagnostics_tx, _diagnostics_rx) = mpsc::channel(1);
        let (crash_tx, _crash_rx) = mpsc::channel(1);
        let client = LspClient::spawn(
            1,
            1,
            LspClientConfig {
                command: script.path().to_string_lossy().into_owned(),
                args: Vec::new(),
                root_path: std::env::temp_dir(),
                request_timeout: Duration::from_secs(2),
                language_id: "test".into(),
                initialization_options: None,
            },
            diagnostics_tx,
            crash_tx,
        )
        .await
        .unwrap();
        client.initialize().await.unwrap();

        let started = std::time::Instant::now();
        assert!(matches!(
            client.shutdown().await,
            Err(LspError::Timeout(_))
        ));
        assert!(started.elapsed() < Duration::from_secs(4));
        assert!(client.child.lock().await.is_none(), "child was reaped");
    }
}
