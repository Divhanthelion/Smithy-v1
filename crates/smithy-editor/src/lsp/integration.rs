//! LSP integration module
//!
//! This module provides the integration layer between the LSP subsystem
//! and the editor's reactive UI using Floem signals and channels.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use crossbeam_channel::{unbounded, Receiver, Sender};
use tokio::sync::mpsc;

use super::{LspCompletion, LspDiagnostic, LspHover, LspRegistry, SharedLspRegistry};

/// Events sent from the editor to the LSP manager
#[derive(Debug, Clone)]
pub enum LspRequest {
    /// Initialize LSP for a workspace
    Initialize { workspace_root: PathBuf },
    /// A file was opened
    FileOpened {
        path: PathBuf,
        language_id: String,
        content: String,
    },
    /// A file was changed
    FileChanged {
        path: PathBuf,
        version: i32,
        content: String,
    },
    /// A file was closed
    FileClosed { path: PathBuf },
    /// Request hover information
    Hover {
        path: PathBuf,
        line: u32,
        character: u32,
        request_id: u64,
    },
    /// Request completions
    Completion {
        path: PathBuf,
        line: u32,
        character: u32,
        request_id: u64,
    },
    /// Request go to definition
    GotoDefinition {
        path: PathBuf,
        line: u32,
        character: u32,
        request_id: u64,
    },
    /// Stop the running servers but keep the worker alive.
    ///
    /// Distinct from [`LspRequest::Shutdown`], which also ends the request loop
    /// and so cannot be recovered from — after it, nothing is reading the
    /// channel and an `Initialize` would sit in the queue forever. This is the
    /// one to send when the point is to reclaim memory and start again later:
    /// rust-analyzer's footprint on a large dependency graph is measured in
    /// gigabytes, and there is no reason to hold that while editing a file it
    /// is not helping with.
    StopServers,
    /// Shutdown all language servers **and end the worker**. App exit only.
    Shutdown,
}

/// Events sent from the LSP manager to the editor
#[derive(Debug, Clone)]
pub enum LspResponse {
    /// Hover response
    Hover {
        request_id: u64,
        result: Option<LspHover>,
    },
    /// Completion response
    Completion {
        request_id: u64,
        items: Vec<LspCompletion>,
    },
    /// Go to definition response
    GotoDefinition {
        request_id: u64,
        location: Option<(PathBuf, u32, u32)>, // (file, line, column)
    },
    /// Diagnostics update for a file
    Diagnostics {
        path: PathBuf,
        diagnostics: Vec<LspDiagnostic>,
    },
    /// A language server started and is analysing the workspace.
    ///
    /// Sent so the UI can tell "no problems" from "no server". Without it an
    /// empty Problems panel means both, and the second is the one worth knowing.
    Ready { servers: usize },
    /// An error occurred
    Error {
        request_id: Option<u64>,
        message: String,
    },
    /// Server status changed
    ServerStatus { language: String, running: bool },
}

/// LSP manager that runs in the background and handles all LSP communication
pub struct LspManager {
    /// Registry of language servers
    registry: SharedLspRegistry,
    /// Receiver for requests from the editor
    request_rx: Receiver<LspRequest>,
    /// Sender for responses to the editor
    response_tx: Sender<LspResponse>,
    /// Tokio runtime for async operations
    runtime: tokio::runtime::Handle,
    /// Channel for receiving diagnostics from LSP clients
    diagnostics_rx: Option<mpsc::Receiver<(String, Vec<LspDiagnostic>)>>,
    /// Sender for diagnostics (passed to clients)
    diagnostics_tx: mpsc::Sender<(String, Vec<LspDiagnostic>)>,
}

impl LspManager {
    /// Create a new LSP manager with channels for communication
    pub fn new(
        runtime: tokio::runtime::Handle,
    ) -> (Self, Sender<LspRequest>, Receiver<LspResponse>) {
        let (request_tx, request_rx) = unbounded();
        let (response_tx, response_rx) = unbounded();
        let (diagnostics_tx, diagnostics_rx) = mpsc::channel(100);

        let manager = Self {
            registry: LspRegistry::new_shared(),
            request_rx,
            response_tx,
            runtime,
            diagnostics_rx: Some(diagnostics_rx),
            diagnostics_tx,
        };

        (manager, request_tx, response_rx)
    }

    /// Run the LSP manager (blocking, should run in background thread)
    pub fn run(mut self) {
        // Take ownership of diagnostics receiver
        let mut diagnostics_rx = self.diagnostics_rx.take().unwrap();
        let response_tx_for_diagnostics = self.response_tx.clone();

        // Spawn diagnostics handler on tokio runtime
        self.runtime.spawn(async move {
            while let Some((uri, diagnostics)) = diagnostics_rx.recv().await {
                // Convert URI to path
                if let Some(path) = uri_to_path(&uri) {
                    let _ = response_tx_for_diagnostics
                        .send(LspResponse::Diagnostics { path, diagnostics });
                }
            }
        });

        // Take ownership of crash receiver from registry
        let crash_rx = {
            let mut registry = self.runtime.block_on(self.registry.write());
            registry.crash_rx.take()
        };

        // Spawn crash handler on tokio runtime
        if let Some(mut crash_rx) = crash_rx {
            let registry = self.registry.clone();
            self.runtime.spawn(async move {
                while let Some(client_id) = crash_rx.recv().await {
                    let mut reg = registry.write().await;
                    reg.handle_crash(client_id).await;
                }
            });
        }

        // Main request handling loop. A `recv` error means every sender is gone,
        // which is itself a shutdown signal — hence `while let` rather than an
        // explicit error arm.
        while let Ok(request) = self.request_rx.recv() {
            if matches!(request, LspRequest::Shutdown) {
                self.handle_shutdown();
                break;
            }
            self.handle_request(request);
        }
    }

    /// Handle a request from the editor
    fn handle_request(&self, request: LspRequest) {
        let registry = self.registry.clone();
        let response_tx = self.response_tx.clone();
        let diagnostics_tx = self.diagnostics_tx.clone();

        match request {
            LspRequest::Initialize { workspace_root } => {
                self.runtime.spawn(async move {
                    let mut reg = registry.write().await;
                    match reg
                        .initialize_for_workspace(&workspace_root, diagnostics_tx)
                        .await
                    {
                        Err(e) => {
                            let _ = response_tx.send(LspResponse::Error {
                                request_id: None,
                                message: format!("Failed to initialize LSP: {}", e),
                            });
                        }
                        Ok(()) => {
                            let _ = response_tx.send(LspResponse::Ready {
                                servers: reg.client_count(),
                            });
                        }
                    }
                });
            }
            LspRequest::FileOpened {
                path,
                language_id,
                content,
            } => {
                self.runtime.spawn(async move {
                    let reg = registry.read().await;
                    if let Some(client) = reg.current_client_for(&language_id) {
                        let uri = path_to_uri(&path);
                        if let Err(e) = client.did_open(&uri, &language_id, &content).await {
                            let _ = response_tx.send(LspResponse::Error {
                                request_id: None,
                                message: format!("LSP didOpen failed: {}", e),
                            });
                        }
                    }
                });
            }
            LspRequest::FileChanged {
                path,
                version,
                content,
            } => {
                self.runtime.spawn(async move {
                    let language_id = language_from_path(&path);
                    let reg = registry.read().await;
                    if let Some(client) = reg.current_client_for(&language_id) {
                        let uri = path_to_uri(&path);
                        if let Err(e) = client.did_change(&uri, version, &content).await {
                            let _ = response_tx.send(LspResponse::Error {
                                request_id: None,
                                message: format!("LSP didChange failed: {}", e),
                            });
                        }
                    }
                });
            }
            LspRequest::FileClosed { path } => {
                self.runtime.spawn(async move {
                    let language_id = language_from_path(&path);
                    let reg = registry.read().await;
                    if let Some(client) = reg.current_client_for(&language_id) {
                        let uri = path_to_uri(&path);
                        let _ = client.did_close(&uri).await;
                    }
                });
            }
            LspRequest::Hover {
                path,
                line,
                character,
                request_id,
            } => {
                self.runtime.spawn(async move {
                    let language_id = language_from_path(&path);
                    let reg = registry.read().await;

                    let result = if let Some(client) = reg.current_client_for(&language_id) {
                        let uri = path_to_uri(&path);
                        match client.hover(&uri, line, character).await {
                            Ok(hover) => hover,
                            Err(e) => {
                                let _ = response_tx.send(LspResponse::Error {
                                    request_id: Some(request_id),
                                    message: format!("Hover request failed: {}", e),
                                });
                                return;
                            }
                        }
                    } else {
                        None
                    };

                    let _ = response_tx.send(LspResponse::Hover { request_id, result });
                });
            }
            LspRequest::Completion {
                path,
                line,
                character,
                request_id,
            } => {
                self.runtime.spawn(async move {
                    let language_id = language_from_path(&path);
                    let reg = registry.read().await;

                    let items = if let Some(client) = reg.current_client_for(&language_id) {
                        let uri = path_to_uri(&path);
                        match client.completion(&uri, line, character).await {
                            Ok(completions) => completions,
                            Err(e) => {
                                let _ = response_tx.send(LspResponse::Error {
                                    request_id: Some(request_id),
                                    message: format!("Completion request failed: {}", e),
                                });
                                return;
                            }
                        }
                    } else {
                        vec![]
                    };

                    let _ = response_tx.send(LspResponse::Completion { request_id, items });
                });
            }
            LspRequest::GotoDefinition {
                path,
                line,
                character,
                request_id,
            } => {
                self.runtime.spawn(async move {
                    let language_id = language_from_path(&path);
                    let reg = registry.read().await;

                    let location = if let Some(client) = reg.current_client_for(&language_id) {
                        let uri = path_to_uri(&path);
                        match client.goto_definition(&uri, line, character).await {
                            Ok(Some(loc)) => uri_to_path(&loc.uri.to_string())
                                .map(|p| (p, loc.range.start.line, loc.range.start.character)),
                            Ok(None) => None,
                            Err(e) => {
                                let _ = response_tx.send(LspResponse::Error {
                                    request_id: Some(request_id),
                                    message: format!("Go to definition failed: {}", e),
                                });
                                return;
                            }
                        }
                    } else {
                        None
                    };

                    let _ = response_tx.send(LspResponse::GotoDefinition {
                        request_id,
                        location,
                    });
                });
            }
            LspRequest::StopServers => {
                self.handle_shutdown();
            }
            LspRequest::Shutdown => {
                // Handled in main loop
            }
        }
    }

    /// Handle shutdown request
    fn handle_shutdown(&self) {
        let registry = self.registry.clone();
        self.runtime.block_on(async move {
            let mut reg = registry.write().await;
            reg.shutdown_all().await;
        });
    }
}

/// Convert a file path to a URI string
fn path_to_uri(path: &std::path::Path) -> String {
    super::uri::path_to_uri(path)
}

/// Convert a URI string to a file path
fn uri_to_path(uri: &str) -> Option<PathBuf> {
    super::uri::uri_to_path(uri)
}

/// Determine language ID from file extension
fn language_from_path(path: &std::path::Path) -> String {
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| match ext {
            "rs" => "rust",
            "ts" | "tsx" => "typescript",
            "js" | "jsx" => "javascript",
            "py" => "python",
            "go" => "go",
            "c" | "h" => "c",
            "cpp" | "cc" | "cxx" | "hpp" | "hxx" => "cpp",
            "java" => "java",
            "rb" => "ruby",
            "php" => "php",
            "swift" => "swift",
            "kt" | "kts" => "kotlin",
            "scala" => "scala",
            "lua" => "lua",
            "hs" => "haskell",
            "ml" | "mli" => "ocaml",
            "ex" | "exs" => "elixir",
            "erl" => "erlang",
            "clj" | "cljs" => "clojure",
            "cs" => "csharp",
            "fs" | "fsx" => "fsharp",
            "v" | "sv" => "verilog",
            "vhd" | "vhdl" => "vhdl",
            "json" => "json",
            "yaml" | "yml" => "yaml",
            "toml" => "toml",
            "xml" => "xml",
            "html" | "htm" => "html",
            "css" => "css",
            "scss" | "sass" => "scss",
            "md" | "markdown" => "markdown",
            "sh" | "bash" => "shellscript",
            "ps1" => "powershell",
            "sql" => "sql",
            "dockerfile" => "dockerfile",
            _ => ext,
        })
        .unwrap_or("plaintext")
        .to_string()
}

/// LSP handle for use in the UI thread
///
/// This provides a synchronous API for sending LSP requests from the UI.
#[derive(Clone)]
pub struct LspHandle {
    request_tx: Sender<LspRequest>,
    next_request_id: Arc<AtomicU64>,
}

impl LspHandle {
    /// Create a new LSP handle
    pub fn new(request_tx: Sender<LspRequest>) -> Self {
        Self {
            request_tx,
            next_request_id: Arc::new(AtomicU64::new(1)),
        }
    }

    /// Get the next request ID
    fn next_id(&self) -> u64 {
        self.next_request_id.fetch_add(1, Ordering::SeqCst)
    }

    /// Initialize LSP for a workspace
    pub fn initialize(&self, workspace_root: PathBuf) {
        let _ = self
            .request_tx
            .send(LspRequest::Initialize { workspace_root });
    }

    /// Notify that a file was opened
    pub fn file_opened(&self, path: PathBuf, language_id: String, content: String) {
        let _ = self.request_tx.send(LspRequest::FileOpened {
            path,
            language_id,
            content,
        });
    }

    /// Notify that a file was changed
    pub fn file_changed(&self, path: PathBuf, version: i32, content: String) {
        let _ = self.request_tx.send(LspRequest::FileChanged {
            path,
            version,
            content,
        });
    }

    /// Request hover information
    ///
    /// Returns a request ID that will be included in the response
    pub fn hover(&self, path: PathBuf, line: u32, character: u32) -> u64 {
        let request_id = self.next_id();
        let _ = self.request_tx.send(LspRequest::Hover {
            path,
            line,
            character,
            request_id,
        });
        request_id
    }

    /// Request completions
    ///
    /// Returns a request ID that will be included in the response
    pub fn completion(&self, path: PathBuf, line: u32, character: u32) -> u64 {
        let request_id = self.next_id();
        let _ = self.request_tx.send(LspRequest::Completion {
            path,
            line,
            character,
            request_id,
        });
        request_id
    }

    /// Request go to definition
    ///
    /// Returns a request ID that will be included in the response
    pub fn goto_definition(&self, path: PathBuf, line: u32, character: u32) -> u64 {
        let request_id = self.next_id();
        let _ = self.request_tx.send(LspRequest::GotoDefinition {
            path,
            line,
            character,
            request_id,
        });
        request_id
    }

    /// Shutdown all language servers
    pub fn shutdown(&self) {
        let _ = self.request_tx.send(LspRequest::Shutdown);
    }

    /// Stop the language servers, keeping the worker ready for a restart.
    ///
    /// Reclaims the analyzer's memory — gigabytes, on a large dependency graph
    /// — without ending the session. Call [`LspHandle::initialize`] to bring it
    /// back.
    pub fn stop_servers(&self) {
        let _ = self.request_tx.send(LspRequest::StopServers);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_path_selects_its_language_id() {
        assert_eq!(language_from_path(std::path::Path::new("test.rs")), "rust");
        assert_eq!(
            language_from_path(std::path::Path::new("test.py")),
            "python"
        );
        assert_eq!(
            language_from_path(std::path::Path::new("test.ts")),
            "typescript"
        );
        assert_eq!(
            language_from_path(std::path::Path::new("test.js")),
            "javascript"
        );
        assert_eq!(language_from_path(std::path::Path::new("test.go")), "go");
        assert_eq!(language_from_path(std::path::Path::new("test.cpp")), "cpp");
        assert_eq!(
            language_from_path(std::path::Path::new("test.unknown")),
            "unknown"
        );
        assert_eq!(
            language_from_path(std::path::Path::new("noext")),
            "plaintext"
        );
    }

    #[test]
    fn a_path_becomes_a_file_uri() {
        #[cfg(windows)]
        {
            let path = std::path::Path::new("C:\\Users\\test\\file.rs");
            let uri = path_to_uri(path);
            assert!(uri.starts_with("file:///"));
            assert!(uri.contains("Users/test/file.rs"));
        }
        #[cfg(not(windows))]
        {
            let path = std::path::Path::new("/home/test/file.rs");
            let uri = path_to_uri(path);
            assert_eq!(uri, "file:///home/test/file.rs");
        }
    }

    #[test]
    fn a_file_uri_becomes_a_path_again() {
        #[cfg(windows)]
        {
            let uri = "file:///C:/Users/test/file.rs";
            let path = uri_to_path(uri).unwrap();
            assert!(path.to_string_lossy().contains("Users"));
        }
        #[cfg(not(windows))]
        {
            let uri = "file:///home/test/file.rs";
            let path = uri_to_path(uri).unwrap();
            assert_eq!(path, PathBuf::from("/home/test/file.rs"));
        }
    }
}
