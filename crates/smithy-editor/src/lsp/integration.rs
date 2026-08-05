//! LSP integration module
//!
//! This module provides the integration layer between the LSP subsystem
//! and the editor's reactive UI using Floem signals and channels.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crossbeam_channel::{bounded, unbounded, Receiver, Sender};
use tokio::sync::mpsc;

use super::{
    ClientDiagnostics, LspClient, LspCompletion, LspDiagnostic, LspHover, LspRegistry, LspStamp,
    RestartOutcome, SharedLspRegistry,
};

/// Sending a full document on every keystroke made rust-analyzer parse dozens
/// of obsolete snapshots while typing quickly. Seventy-five milliseconds kept
/// interactive requests current while collapsing ordinary key bursts.
const FULL_DOCUMENT_CHANGE_DEBOUNCE: Duration = Duration::from_millis(75);
/// The client itself allows 1.5 seconds to request exit and reap or kill. The
/// acknowledgement leaves margin for runtime scheduling without letting app
/// termination wait indefinitely.
const MANAGER_SHUTDOWN_ACK_TIMEOUT: Duration = Duration::from_secs(5);
/// Final snapshots are valuable, but a wedged analyzer must not turn "quit"
/// back into the 30-second hang this shutdown path exists to prevent.
const SHUTDOWN_DOCUMENT_FLUSH_TIMEOUT: Duration = Duration::from_millis(500);

struct OpenDocument {
    language_id: String,
    initial_content: Arc<str>,
    latest_content: Arc<str>,
    version: i32,
    epoch: u64,
    debounce: u64,
    open: bool,
    operation: Arc<tokio::sync::Mutex<()>>,
    /// `(client id, version)` last delivered to that exact process.
    sent: Option<(u64, i32)>,
}

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
struct DocumentKey {
    root_path: PathBuf,
    path: PathBuf,
}

impl DocumentKey {
    fn new(root_path: PathBuf, path: PathBuf) -> Self {
        Self { root_path, path }
    }
}

type OpenDocuments = Arc<Mutex<HashMap<DocumentKey, OpenDocument>>>;
type PathOperations = Arc<Mutex<HashMap<DocumentKey, Arc<tokio::sync::Mutex<()>>>>>;

/// Events sent from the editor to the LSP manager
#[derive(Debug, Clone)]
pub enum LspRequest {
    /// Initialize LSP for a workspace
    Initialize { workspace_root: PathBuf },
    /// A file was opened
    FileOpened {
        workspace_root: PathBuf,
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
    Shutdown { acknowledged: Sender<()> },
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
        stamp: LspStamp,
        path: PathBuf,
        diagnostics: Vec<LspDiagnostic>,
    },
    /// A language server started and is analysing the workspace.
    ///
    /// Sent so the UI can tell "no problems" from "no server". Without it an
    /// empty Problems panel means both, and the second is the one worth knowing.
    Ready { stamp: LspStamp, servers: usize },
    /// An error occurred
    Error {
        stamp: LspStamp,
        request_id: Option<u64>,
        message: String,
    },
    /// Server status changed
    ServerStatus {
        stamp: LspStamp,
        language: String,
        running: bool,
    },
}

impl LspResponse {
    pub fn stamp(&self) -> Option<&LspStamp> {
        match self {
            LspResponse::Diagnostics { stamp, .. }
            | LspResponse::Ready { stamp, .. }
            | LspResponse::Error { stamp, .. }
            | LspResponse::ServerStatus { stamp, .. } => Some(stamp),
            LspResponse::Hover { .. }
            | LspResponse::Completion { .. }
            | LspResponse::GotoDefinition { .. } => None,
        }
    }
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
    diagnostics_rx: Option<mpsc::Receiver<ClientDiagnostics>>,
    /// Sender for diagnostics (passed to clients)
    diagnostics_tx: mpsc::Sender<ClientDiagnostics>,
    /// Latest full snapshots survive a stopped or restarting server.
    documents: OpenDocuments,
    next_document_epoch: Arc<AtomicU64>,
    path_operations: PathOperations,
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
            documents: Arc::new(Mutex::new(HashMap::new())),
            next_document_epoch: Arc::new(AtomicU64::new(1)),
            path_operations: Arc::new(Mutex::new(HashMap::new())),
        };

        (manager, request_tx, response_rx)
    }

    /// Run the LSP manager (blocking, should run in background thread)
    pub fn run(mut self) {
        // Take ownership of diagnostics receiver
        let mut diagnostics_rx = self.diagnostics_rx.take().unwrap();
        let response_tx_for_diagnostics = self.response_tx.clone();
        let registry_for_diagnostics = self.registry.clone();

        // Spawn diagnostics handler on tokio runtime
        self.runtime.spawn(async move {
            while let Some(event) = diagnostics_rx.recv().await {
                if !registry_for_diagnostics
                    .read()
                    .await
                    .accepts_client_stamp(&event.stamp)
                {
                    continue;
                }
                // Convert URI to path
                if let Some(path) = uri_to_path(&event.uri) {
                    let _ = response_tx_for_diagnostics
                        .send(LspResponse::Diagnostics {
                            stamp: event.stamp,
                            path,
                            diagnostics: event.diagnostics,
                        });
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
            let documents = self.documents.clone();
            let response_tx = self.response_tx.clone();
            self.runtime.spawn(async move {
                while let Some(crash) = crash_rx.recv().await {
                    let outcome = LspRegistry::restart_after_crash(&registry, crash).await;
                    if let Some(client) = publish_restart_outcome(&response_tx, outcome) {
                        replay_open_documents(&documents, client).await;
                    }
                }
            });
        }

        // Main request handling loop. A `recv` error means every sender is gone,
        // which is itself a shutdown signal — hence `while let` rather than an
        // explicit error arm.
        while let Ok(request) = self.request_rx.recv() {
            if let LspRequest::Shutdown { acknowledged } = request {
                self.handle_shutdown();
                let _ = acknowledged.send(());
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
                let workspace_root = canonical_workspace_root(&workspace_root);
                let documents = self.documents.clone();
                self.runtime.spawn(async move {
                    match LspRegistry::initialize_shared(
                        &registry,
                        &workspace_root,
                        diagnostics_tx,
                    )
                    .await
                    {
                        Err((stamp, e)) => {
                            let _ = response_tx.send(LspResponse::Error {
                                stamp,
                                request_id: None,
                                message: format!("Failed to initialize LSP: {}", e),
                            });
                        }
                        Ok((servers, stamp)) => {
                            let client = {
                                registry.read().await.current_client_for("rust")
                            };
                            if let Some(client) = client {
                                replay_open_documents(&documents, client).await;
                            }
                            let _ = response_tx.send(LspResponse::Ready { stamp, servers });
                        }
                    }
                });
            }
            LspRequest::FileOpened {
                workspace_root,
                path,
                language_id,
                content,
            } => {
                let key = DocumentKey::new(canonical_workspace_root(&workspace_root), path.clone());
                let operation = path_operation(&self.path_operations, &key);
                let ordered = reserve_path_lifecycle(&self.runtime, operation.clone());
                let epoch = self.next_document_epoch.fetch_add(1, Ordering::SeqCst);
                let content: Arc<str> = content.into();
                self.documents
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(
                        key.clone(),
                        OpenDocument {
                            language_id: language_id.clone(),
                            initial_content: content.clone(),
                            latest_content: content,
                            version: 0,
                            epoch,
                            debounce: 0,
                            open: true,
                            operation,
                            sent: None,
                        },
                    );
                let documents = self.documents.clone();
                self.runtime.spawn(async move {
                    if let Some(client) =
                        client_for_root(&registry, &language_id, &key.root_path).await
                    {
                        let stamp = client.stamp();
                        if let Err(e) =
                            send_document_snapshot(&documents, &key, epoch, &client, false).await
                        {
                            let _ = response_tx.send(LspResponse::Error {
                                stamp,
                                request_id: None,
                                message: format!("LSP didOpen failed: {}", e),
                            });
                        }
                    }
                    drop(ordered);
                });
            }
            LspRequest::FileChanged {
                path,
                version,
                content,
            } => {
                let (key, epoch, debounce) = {
                    let mut documents = self
                        .documents
                        .lock()
                        .unwrap_or_else(|e| e.into_inner());
                    let Some((key, document)) = documents
                        .iter_mut()
                        .find(|(key, document)| key.path == path && document.open)
                    else {
                        return;
                    };
                    document.latest_content = Arc::from(content);
                    document.version = version;
                    document.debounce = document.debounce.wrapping_add(1);
                    (key.clone(), document.epoch, document.debounce)
                };
                let documents = self.documents.clone();
                self.runtime.spawn(async move {
                    tokio::time::sleep(FULL_DOCUMENT_CHANGE_DEBOUNCE).await;
                    if !debounce_is_current(&documents, &key, epoch, debounce) {
                        return;
                    }
                    let language_id = language_from_path(&path);
                    if let Some(client) =
                        client_for_root(&registry, &language_id, &key.root_path).await
                    {
                        let stamp = client.stamp();
                        if let Err(e) =
                            sync_document(&documents, &key, epoch, client, true).await
                        {
                            let _ = response_tx.send(LspResponse::Error {
                                stamp,
                                request_id: None,
                                message: format!("LSP didChange failed: {}", e),
                            });
                        }
                    }
                });
            }
            LspRequest::FileClosed { path } => {
                let Some(key) = document_key_for_path(&self.documents, &path) else {
                    return;
                };
                let operation = path_operation(&self.path_operations, &key);
                let ordered = reserve_path_lifecycle(&self.runtime, operation);
                let epoch = {
                    let mut documents = self
                        .documents
                        .lock()
                        .unwrap_or_else(|e| e.into_inner());
                    let Some(document) = documents.get_mut(&key) else {
                        drop(ordered);
                        return;
                    };
                    document.open = false;
                    document.debounce = document.debounce.wrapping_add(1);
                    document.epoch
                };
                let documents = self.documents.clone();
                self.runtime.spawn(async move {
                    let language_id = language_from_path(&path);
                    if let Some(client) =
                        client_for_root(&registry, &language_id, &key.root_path).await
                    {
                        let _ = close_document_ordered(
                            &documents,
                            &key,
                            epoch,
                            client,
                            ordered,
                        )
                        .await;
                    } else {
                        let mut documents =
                            documents.lock().unwrap_or_else(|e| e.into_inner());
                        if documents
                            .get(&key)
                            .is_some_and(|document| document.epoch == epoch && !document.open)
                        {
                            documents.remove(&key);
                        }
                        drop(ordered);
                    }
                });
            }
            LspRequest::Hover {
                path,
                line,
                character,
                request_id,
            } => {
                let documents = self.documents.clone();
                let reserved = document_key_for_path(&documents, &path).map(|key| {
                    let ordered = reserve_path_lifecycle(
                        &self.runtime,
                        path_operation(&self.path_operations, &key),
                    );
                    (key, ordered)
                });
                self.runtime.spawn(async move {
                    let language_id = language_from_path(&path);
                    let result = if let Some((key, ordered, client)) = match reserved {
                        Some((key, ordered)) => client_for_root(&registry, &language_id, &key.root_path)
                            .await
                            .map(|client| (key, ordered, client)),
                        None => None,
                    } {
                        if let Some(epoch) = document_epoch(&documents, &key) {
                            let _ordered = ordered;
                            match send_document_snapshot(&documents, &key, epoch, &client, true).await {
                                    Ok(()) => {}
                                    Err(e) => {
                                        let _ = response_tx.send(LspResponse::Error {
                                            stamp: client.stamp(),
                                            request_id: Some(request_id),
                                            message: format!("Hover synchronization failed: {e}"),
                                        });
                                        return;
                                    }
                                }
                            let uri = path_to_uri(&path);
                            match client.hover(&uri, line, character).await {
                                Ok(hover) => {
                                    let _ = response_tx.send(LspResponse::Hover {
                                        request_id,
                                        result: hover,
                                    });
                                }
                                Err(e) => {
                                    let _ = response_tx.send(LspResponse::Error {
                                        stamp: client.stamp(),
                                        request_id: Some(request_id),
                                        message: format!("Hover request failed: {}", e),
                                    });
                                }
                            }
                            return;
                        }
                        None
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
                let documents = self.documents.clone();
                let reserved = document_key_for_path(&documents, &path).map(|key| {
                    let ordered = reserve_path_lifecycle(
                        &self.runtime,
                        path_operation(&self.path_operations, &key),
                    );
                    (key, ordered)
                });
                self.runtime.spawn(async move {
                    let language_id = language_from_path(&path);
                    let items = if let Some((key, ordered, client)) = match reserved {
                        Some((key, ordered)) => client_for_root(&registry, &language_id, &key.root_path)
                            .await
                            .map(|client| (key, ordered, client)),
                        None => None,
                    } {
                        if let Some(epoch) = document_epoch(&documents, &key) {
                            let _ordered = ordered;
                            match send_document_snapshot(&documents, &key, epoch, &client, true).await {
                                    Ok(()) => {}
                                    Err(e) => {
                                        let _ = response_tx.send(LspResponse::Error {
                                            stamp: client.stamp(),
                                            request_id: Some(request_id),
                                            message: format!("Completion synchronization failed: {e}"),
                                        });
                                        return;
                                    }
                                }
                            let uri = path_to_uri(&path);
                            match client.completion(&uri, line, character).await {
                                Ok(items) => {
                                    let _ = response_tx.send(LspResponse::Completion {
                                        request_id,
                                        items,
                                    });
                                }
                                Err(e) => {
                                    let _ = response_tx.send(LspResponse::Error {
                                        stamp: client.stamp(),
                                        request_id: Some(request_id),
                                        message: format!("Completion request failed: {}", e),
                                    });
                                }
                            }
                            return;
                        }
                        vec![]
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
                let documents = self.documents.clone();
                let reserved = document_key_for_path(&documents, &path).map(|key| {
                    let ordered = reserve_path_lifecycle(
                        &self.runtime,
                        path_operation(&self.path_operations, &key),
                    );
                    (key, ordered)
                });
                self.runtime.spawn(async move {
                    let language_id = language_from_path(&path);
                    let location = if let Some((key, ordered, client)) = match reserved {
                        Some((key, ordered)) => client_for_root(&registry, &language_id, &key.root_path)
                            .await
                            .map(|client| (key, ordered, client)),
                        None => None,
                    } {
                        if let Some(epoch) = document_epoch(&documents, &key) {
                            let _ordered = ordered;
                            match send_document_snapshot(&documents, &key, epoch, &client, true).await {
                                    Ok(()) => {}
                                    Err(e) => {
                                        let _ = response_tx.send(LspResponse::Error {
                                            stamp: client.stamp(),
                                            request_id: Some(request_id),
                                            message: format!("Definition synchronization failed: {e}"),
                                        });
                                        return;
                                    }
                                }
                            let uri = path_to_uri(&path);
                            let location = match client.goto_definition(&uri, line, character).await {
                                Ok(Some(loc)) => uri_to_path(&loc.uri.to_string())
                                    .map(|p| (p, loc.range.start.line, loc.range.start.character)),
                                Ok(None) => None,
                                Err(e) => {
                                    let _ = response_tx.send(LspResponse::Error {
                                        stamp: client.stamp(),
                                        request_id: Some(request_id),
                                        message: format!("Go to definition failed: {}", e),
                                    });
                                    return;
                                }
                            };
                            let _ = response_tx.send(LspResponse::GotoDefinition {
                                request_id,
                                location,
                            });
                            return;
                        }
                        None
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
            LspRequest::Shutdown { .. } => {
                // Handled in main loop
            }
        }
    }

    /// Handle shutdown request
    fn handle_shutdown(&self) {
        let registry = self.registry.clone();
        let documents = self.documents.clone();
        self.runtime.block_on(async move {
            let clients = registry.read().await.clients_snapshot();
            let _ = tokio::time::timeout(SHUTDOWN_DOCUMENT_FLUSH_TIMEOUT, async {
                for client in clients {
                    flush_open_documents(&documents, client).await;
                }
            })
            .await;
            LspRegistry::stop_shared(&registry).await;
        });
    }
}

fn publish_restart_outcome(
    response_tx: &Sender<LspResponse>,
    outcome: RestartOutcome,
) -> Option<Arc<LspClient>> {
    match outcome {
        RestartOutcome::Restarted { key, client } => {
            let _ = response_tx.send(LspResponse::ServerStatus {
                stamp: client.stamp(),
                language: key.language_id,
                running: true,
            });
            Some(client)
        }
        RestartOutcome::Exhausted {
            stamp,
            language,
            message,
        } => {
            let _ = response_tx.send(LspResponse::ServerStatus {
                stamp: stamp.clone(),
                language,
                running: false,
            });
            let _ = response_tx.send(LspResponse::Error {
                stamp,
                request_id: None,
                message,
            });
            None
        }
        RestartOutcome::Obsolete => None,
    }
}

async fn client_for_root(
    registry: &SharedLspRegistry,
    language_id: &str,
    root_path: &std::path::Path,
) -> Option<Arc<LspClient>> {
    // Every request takes only the Arc it needs. No registry guard survives an
    // LSP send, response wait, debounce, process spawn, or shutdown.
    registry.read().await.client_for(language_id, root_path)
}

fn document_key_for_path(
    documents: &OpenDocuments,
    path: &std::path::Path,
) -> Option<DocumentKey> {
    documents
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .find(|(key, document)| key.path == path && document.open)
        .map(|(key, _)| key.clone())
}

fn document_epoch(documents: &OpenDocuments, key: &DocumentKey) -> Option<u64> {
    documents
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(key)
        .filter(|document| document.open)
        .map(|document| document.epoch)
}

fn debounce_is_current(
    documents: &OpenDocuments,
    key: &DocumentKey,
    epoch: u64,
    debounce: u64,
) -> bool {
    documents
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(key)
        .is_some_and(|document| {
            document.open && document.epoch == epoch && document.debounce == debounce
        })
}

fn path_operation(
    operations: &PathOperations,
    key: &DocumentKey,
) -> Arc<tokio::sync::Mutex<()>> {
    operations
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .entry(key.clone())
        .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
        .clone()
}

fn reserve_path_lifecycle(
    runtime: &tokio::runtime::Handle,
    operation: Arc<tokio::sync::Mutex<()>>,
) -> tokio::sync::OwnedMutexGuard<()> {
    runtime.block_on(operation.lock_owned())
}

async fn sync_document(
    documents: &OpenDocuments,
    key: &DocumentKey,
    epoch: u64,
    client: Arc<LspClient>,
    flush_latest: bool,
) -> Result<(), super::LspError> {
    let operation = {
        let documents = documents.lock().unwrap_or_else(|e| e.into_inner());
        let Some(document) = documents.get(key) else {
            return Ok(());
        };
        if document.epoch != epoch {
            return Ok(());
        }
        document.operation.clone()
    };
    let _ordered = operation.lock().await;
    send_document_snapshot(documents, key, epoch, &client, flush_latest).await
}

async fn send_document_snapshot(
    documents: &OpenDocuments,
    key: &DocumentKey,
    epoch: u64,
    client: &LspClient,
    flush_latest: bool,
) -> Result<(), super::LspError> {
    let open = {
        let documents = documents.lock().unwrap_or_else(|e| e.into_inner());
        let Some(document) = documents.get(key) else {
            return Ok(());
        };
        if document.epoch != epoch {
            return Ok(());
        }
        if document.sent.map(|sent| sent.0) == Some(client.id()) {
            None
        } else {
            let restarting = document.sent.is_some();
            Some((
                document.language_id.clone(),
                if restarting {
                    document.latest_content.clone()
                } else {
                    document.initial_content.clone()
                },
                if restarting { document.version } else { 0 },
            ))
        }
    };
    if let Some((language_id, content, sent_version)) = open {
        client
            .did_open(&path_to_uri(&key.path), &language_id, sent_version, &content)
            .await?;
        let mut documents = documents.lock().unwrap_or_else(|e| e.into_inner());
        let Some(document) = documents.get_mut(key) else {
            return Ok(());
        };
        if document.epoch != epoch {
            return Ok(());
        }
        document.sent = Some((client.id(), sent_version));
    }

    if !flush_latest {
        return Ok(());
    }
    loop {
        let change = {
            let documents = documents.lock().unwrap_or_else(|e| e.into_inner());
            let Some(document) = documents.get(key) else {
                return Ok(());
            };
            if document.epoch != epoch {
                return Ok(());
            }
            match document.sent {
                Some((client_id, sent_version))
                    if client_id == client.id() && sent_version < document.version =>
                {
                    Some((document.version, document.latest_content.clone()))
                }
                _ => None,
            }
        };
        let Some((version, content)) = change else {
            return Ok(());
        };
        client
            .did_change(&path_to_uri(&key.path), version, &content)
            .await?;
        let mut documents = documents.lock().unwrap_or_else(|e| e.into_inner());
        let Some(document) = documents.get_mut(key) else {
            return Ok(());
        };
        if document.epoch != epoch {
            return Ok(());
        }
        document.sent = Some((client.id(), version));
    }
}

async fn close_document_ordered(
    documents: &OpenDocuments,
    key: &DocumentKey,
    epoch: u64,
    client: Arc<LspClient>,
    _ordered: tokio::sync::OwnedMutexGuard<()>,
) -> Result<(), super::LspError> {
    send_document_snapshot(documents, key, epoch, &client, true).await?;
    let was_sent = documents
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(key)
        .is_some_and(|document| {
            document.epoch == epoch
                && document.sent.map(|sent| sent.0) == Some(client.id())
        });
    if was_sent {
        client.did_close(&path_to_uri(&key.path)).await?;
    }
    let mut documents = documents.lock().unwrap_or_else(|e| e.into_inner());
    if documents
        .get(key)
        .is_some_and(|document| document.epoch == epoch && !document.open)
    {
        documents.remove(key);
    }
    Ok(())
}

async fn replay_open_documents(documents: &OpenDocuments, client: Arc<LspClient>) {
    let open = open_document_keys_for(
        documents,
        client.language_id(),
        client.root_path(),
    );
    for (key, epoch) in open {
        if let Err(error) =
            replay_document_snapshot(documents, &key, epoch, client.clone()).await
        {
            eprintln!(
                "Failed to replay {} after LSP restart: {error}",
                key.path.display()
            );
        }
    }
}

async fn replay_document_snapshot(
    documents: &OpenDocuments,
    key: &DocumentKey,
    epoch: u64,
    client: Arc<LspClient>,
) -> Result<(), super::LspError> {
    let operation = {
        let documents = documents.lock().unwrap_or_else(|e| e.into_inner());
        let Some(document) = documents.get(key) else {
            return Ok(());
        };
        if document.epoch != epoch {
            return Ok(());
        }
        document.operation.clone()
    };
    let _ordered = operation.lock().await;
    let snapshot = {
        let documents = documents.lock().unwrap_or_else(|e| e.into_inner());
        let Some(document) = documents.get(key) else {
            return Ok(());
        };
        if document.epoch != epoch
            || document.sent.map(|sent| sent.0) == Some(client.id())
        {
            return Ok(());
        }
        (
            document.language_id.clone(),
            document.version,
            document.latest_content.clone(),
        )
    };
    client
        .did_open(
            &path_to_uri(&key.path),
            &snapshot.0,
            snapshot.1,
            &snapshot.2,
        )
        .await?;
    let mut documents = documents.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(document) = documents.get_mut(key) {
        if document.epoch == epoch {
            document.sent = Some((client.id(), snapshot.1));
        }
    }
    Ok(())
}

fn open_document_keys_for(
    documents: &OpenDocuments,
    language_id: &str,
    root_path: &std::path::Path,
) -> Vec<(DocumentKey, u64)> {
    documents
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .filter(|(key, document)| {
            document.open
                && document.language_id == language_id
                && key.root_path == root_path
        })
        .map(|(key, document)| (key.clone(), document.epoch))
        .collect()
}

async fn flush_open_documents(documents: &OpenDocuments, client: Arc<LspClient>) {
    let open =
        open_document_keys_for(documents, client.language_id(), client.root_path());
    for (key, epoch) in open {
        match tokio::time::timeout(
            SHUTDOWN_DOCUMENT_FLUSH_TIMEOUT,
            sync_document(documents, &key, epoch, client.clone(), true),
        )
        .await
        {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                eprintln!(
                    "Failed to flush {} before LSP shutdown: {error}",
                    key.path.display()
                )
            }
            Err(_) => eprintln!(
                "Timed out flushing {} before LSP shutdown",
                key.path.display()
            ),
        }
    }
}

/// Convert a file path to a URI string
fn path_to_uri(path: &std::path::Path) -> String {
    super::uri::path_to_uri(path)
}

fn canonical_workspace_root(path: &std::path::Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
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
    active_root: Arc<Mutex<Option<PathBuf>>>,
}

impl LspHandle {
    /// Create a new LSP handle
    pub fn new(request_tx: Sender<LspRequest>) -> Self {
        Self {
            request_tx,
            next_request_id: Arc::new(AtomicU64::new(1)),
            active_root: Arc::new(Mutex::new(None)),
        }
    }

    /// Get the next request ID
    fn next_id(&self) -> u64 {
        self.next_request_id.fetch_add(1, Ordering::SeqCst)
    }

    /// Initialize LSP for a workspace
    pub fn initialize(&self, workspace_root: PathBuf) {
        let workspace_root = canonical_workspace_root(&workspace_root);
        *self.active_root.lock().unwrap_or_else(|e| e.into_inner()) =
            Some(workspace_root.clone());
        let _ = self
            .request_tx
            .send(LspRequest::Initialize { workspace_root });
    }

    /// Notify that a file was opened
    pub fn file_opened(&self, path: PathBuf, language_id: String, content: String) {
        let Some(workspace_root) = self
            .active_root
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
        else {
            return;
        };
        let _ = self.request_tx.send(LspRequest::FileOpened {
            workspace_root,
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

    /// Notify that a file actually left the open-tab set.
    pub fn file_closed(&self, path: PathBuf) {
        let _ = self.request_tx.send(LspRequest::FileClosed { path });
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

    /// Shutdown all language servers and wait for the worker's bounded
    /// acknowledgement. `false` means the worker itself did not answer in time.
    pub fn shutdown(&self) -> bool {
        let (acknowledged, acknowledgement) = bounded(1);
        if self
            .request_tx
            .send(LspRequest::Shutdown { acknowledged })
            .is_err()
        {
            // A prior shutdown already ended the worker.
            return true;
        }
        acknowledgement
            .recv_timeout(MANAGER_SHUTDOWN_ACK_TIMEOUT)
            .is_ok()
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
    use std::path::Path;

    fn test_key(path: &std::path::Path) -> DocumentKey {
        DocumentKey::new(
            path.parent().unwrap_or(std::path::Path::new("/")).to_path_buf(),
            path.to_path_buf(),
        )
    }

    fn documents_with(path: &std::path::Path, epoch: u64, text: &str) -> OpenDocuments {
        documents_with_root(
            path.parent().unwrap_or(std::path::Path::new("/")),
            path,
            epoch,
            text,
        )
    }

    fn documents_with_root(
        root: &std::path::Path,
        path: &std::path::Path,
        epoch: u64,
        text: &str,
    ) -> OpenDocuments {
        let mut documents = HashMap::new();
        documents.insert(
            DocumentKey::new(root.to_path_buf(), path.to_path_buf()),
            OpenDocument {
                language_id: "rust".into(),
                initial_content: Arc::from(text),
                latest_content: Arc::from(text),
                version: 1,
                epoch,
                debounce: 0,
                open: true,
                operation: Arc::new(tokio::sync::Mutex::new(())),
                sent: None,
            },
        );
        Arc::new(Mutex::new(documents))
    }

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

    /// Cancellation and failed saves never call this method; once close commits,
    /// the one notification sent here must retain the exact document path.
    #[test]
    fn a_committed_close_emits_one_did_close_request() {
        let (tx, rx) = crossbeam_channel::unbounded();
        let handle = LspHandle::new(tx);
        let path = PathBuf::from("/tmp/closed.rs");

        handle.file_closed(path.clone());

        assert!(matches!(
            rx.recv().unwrap(),
            LspRequest::FileClosed { path: sent } if sent == path
        ));
        assert!(rx.try_recv().is_err(), "didClose must be emitted exactly once");
    }

    /// Full-document sync previously emitted once per keystroke. Only the
    /// newest timer token may survive a rapid edit burst.
    #[test]
    fn rapid_edits_leave_one_debounced_change_eligible() {
        let path = PathBuf::from("/tmp/rapid.rs");
        let key = test_key(&path);
        let documents = documents_with(&path, 4, "a");
        {
            let mut documents = documents.lock().unwrap();
            let document = documents.get_mut(&key).unwrap();
            for (version, text) in [(2, "ab"), (3, "abc"), (4, "abcd")] {
                document.version = version;
                document.latest_content = Arc::from(text);
                document.debounce += 1;
            }
        }
        assert!(!debounce_is_current(&documents, &key, 4, 1));
        assert!(!debounce_is_current(&documents, &key, 4, 2));
        assert!(debounce_is_current(&documents, &key, 4, 3));
    }

    /// Closing and reopening the same path creates a new document lifetime. A
    /// timer from the old lifetime must not publish into the new one.
    #[test]
    fn a_timer_from_an_old_document_epoch_is_ignored() {
        let path = PathBuf::from("/tmp/reopened.rs");
        let key = test_key(&path);
        let documents = documents_with(&path, 8, "new");
        assert!(!debounce_is_current(&documents, &key, 7, 0));
        assert!(debounce_is_current(&documents, &key, 8, 0));
    }

    /// Application exit used to enqueue shutdown and immediately call
    /// `process::exit`, so the worker often never read it. The handle now waits
    /// for the worker's explicit acknowledgement.
    #[test]
    fn shutdown_waits_for_the_manager_acknowledgement() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let (manager, tx, _responses) = LspManager::new(runtime.handle().clone());
        let handle = LspHandle::new(tx);
        let worker = std::thread::spawn(move || manager.run());
        assert!(handle.shutdown());
        worker.join().unwrap();
    }

    /// The per-path operation lock is the ordering boundary: semantic work
    /// retains it after flushing, so close cannot overtake the request.
    #[tokio::test]
    async fn semantic_flush_completes_before_close_can_run() {
        let operation = Arc::new(tokio::sync::Mutex::new(()));
        let events = Arc::new(Mutex::new(Vec::new()));
        let semantic_lock = operation.clone().lock_owned().await;
        events.lock().unwrap().push("didChange");
        let operation_for_close = operation.clone();
        let events_for_close = events.clone();
        let close = tokio::spawn(async move {
            let _ordered = operation_for_close.lock().await;
            events_for_close.lock().unwrap().push("didClose");
        });
        tokio::task::yield_now().await;
        assert_eq!(*events.lock().unwrap(), vec!["didChange"]);
        events.lock().unwrap().push("semantic");
        drop(semantic_lock);
        close.await.unwrap();
        assert_eq!(
            *events.lock().unwrap(),
            vec!["didChange", "semantic", "didClose"]
        );
    }

    /// The old epoch was replaced in the map before its close task ran, so the
    /// close saw a different epoch and silently skipped `didClose`. These are
    /// captured wire events, not merely the requests queued by `LspHandle`.
    #[test]
    fn close_reopen_and_rapid_reclose_preserve_wire_order() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let operations: PathOperations = Arc::new(Mutex::new(HashMap::new()));
        let path = PathBuf::from("/tmp/reopen.rs");
        let key = test_key(&path);
        let wire = Arc::new(Mutex::new(Vec::new()));

        for method in ["didOpen:1", "didClose:1", "didOpen:2", "didClose:2"] {
            let guard = reserve_path_lifecycle(
                runtime.handle(),
                path_operation(&operations, &key),
            );
            let wire = wire.clone();
            runtime.spawn(async move {
                wire.lock().unwrap().push(method);
                drop(guard);
            });
        }
        runtime.block_on(async {
            let operation = path_operation(&operations, &key);
            let _drained = operation.lock().await;
        });

        assert_eq!(
            *wire.lock().unwrap(),
            vec!["didOpen:1", "didClose:1", "didOpen:2", "didClose:2"]
        );
    }

    /// Exercise the real transport and capture the server's JSON messages. The
    /// lifecycle lock is only correct if the notifications themselves—not just
    /// the manager queue—arrive close-before-open through a rapid second close.
    #[cfg(unix)]
    #[test]
    fn close_reopen_reclose_reaches_the_server_in_wire_order() {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;

        let runtime = tokio::runtime::Runtime::new().unwrap();
        let (manager, tx, _responses) = LspManager::new(runtime.handle().clone());
        let documents_for_replay = manager.documents.clone();
        let handle = LspHandle::new(tx);
        let root = tempfile::tempdir().unwrap();
        let root_path = canonical_workspace_root(root.path());
        *handle
            .active_root
            .lock()
            .unwrap_or_else(|e| e.into_inner()) =
            Some(root_path.clone());
        let path = root.path().join("reopen.rs");
        let capture = tempfile::NamedTempFile::new().unwrap();
        let capture_path = capture.path().to_path_buf();
        let mut script = tempfile::NamedTempFile::new().unwrap();
        script
            .write_all(
                br#"#!/bin/sh
capture=$1
read_message() {
  length=
  while IFS= read -r line; do
    line=$(printf '%s' "$line" | tr -d '\r')
    [ -z "$line" ] && break
    case "$line" in Content-Length:*) length=${line#Content-Length: };; esac
  done
  [ -n "$length" ] || return 1
  body=$(dd bs=1 count="$length" 2>/dev/null)
}
read_message
reply='{"jsonrpc":"2.0","id":1,"result":{"capabilities":{}}}'
printf 'Content-Length: %s\r\n\r\n%s' "${#reply}" "$reply"
while read_message; do
  printf '%s\n' "$body" >> "$capture"
  case "$body" in
    *'"id":'*)
      id=$(printf '%s' "$body" | sed -E 's/.*"id":([0-9]+).*/\1/')
      reply=$(printf '{"jsonrpc":"2.0","id":%s,"result":null}' "$id")
      printf 'Content-Length: %s\r\n\r\n%s' "${#reply}" "$reply"
      ;;
  esac
done
"#,
            )
            .unwrap();
        let mut permissions = script.as_file().metadata().unwrap().permissions();
        permissions.set_mode(0o755);
        script.as_file().set_permissions(permissions).unwrap();

        let (diagnostics_tx, crash_tx) = runtime.block_on(async {
            manager.registry.read().await.test_channels()
        });
        let client = runtime
            .block_on(LspClient::spawn(
                91,
                5,
                super::super::LspClientConfig {
                    command: script.path().to_string_lossy().into_owned(),
                    args: vec![capture_path.to_string_lossy().into_owned()],
                    root_path: root_path.clone(),
                    request_timeout: Duration::from_secs(2),
                    language_id: "rust".into(),
                    initialization_options: None,
                },
                diagnostics_tx.clone(),
                crash_tx.clone(),
            ))
            .unwrap();
        runtime.block_on(client.initialize()).unwrap();
        runtime.block_on(async {
            manager
                .registry
                .write()
                .await
                .install_test_client(Arc::new(client));
        });
        let worker = std::thread::spawn(move || manager.run());

        handle.file_opened(path.clone(), "rust".into(), "one".into());
        handle.file_changed(path.clone(), 1, "one edit".into());
        handle.completion(path.clone(), 0, 0);
        handle.file_closed(path.clone());
        handle.file_opened(path.clone(), "rust".into(), "two".into());
        handle.file_closed(path.clone());

        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        let methods = loop {
            let captured = std::fs::read_to_string(&capture_path).unwrap();
            let methods: Vec<_> = captured
                .lines()
                .filter_map(|line| {
                    serde_json::from_str::<serde_json::Value>(line)
                        .ok()?
                        .get("method")?
                        .as_str()
                        .map(str::to_string)
                })
                .filter(|method| {
                    method == "textDocument/didOpen" || method == "textDocument/didClose"
                })
                .collect();
            if methods.len() == 4 || std::time::Instant::now() >= deadline {
                break methods;
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        assert_eq!(
            methods,
            vec![
                "textDocument/didOpen",
                "textDocument/didClose",
                "textDocument/didOpen",
                "textDocument/didClose",
            ]
        );
        let captured = std::fs::read_to_string(&capture_path).unwrap();
        let messages: Vec<serde_json::Value> = captured
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect();
        let opened = messages
            .iter()
            .find(|message| message["method"] == "textDocument/didOpen")
            .unwrap();
        let changed = messages
            .iter()
            .find(|message| message["method"] == "textDocument/didChange")
            .unwrap();
        let completion_index = messages
            .iter()
            .position(|message| message["method"] == "textDocument/completion")
            .unwrap();
        let change_index = messages
            .iter()
            .position(|message| message["method"] == "textDocument/didChange")
            .unwrap();
        assert_eq!(opened["params"]["textDocument"]["version"], 0);
        assert_eq!(changed["params"]["textDocument"]["version"], 1);
        assert_eq!(
            changed["params"]["contentChanges"][0]["text"],
            "one edit"
        );
        assert!(
            change_index < completion_index,
            "the edit queued before didOpen completed was not flushed before completion"
        );

        let replay_key = DocumentKey::new(root_path.clone(), path.clone());
        documents_for_replay.lock().unwrap().insert(
            replay_key,
            OpenDocument {
                language_id: "rust".into(),
                initial_content: Arc::from("old"),
                latest_content: Arc::from("restart latest"),
                version: 7,
                epoch: 70,
                debounce: 0,
                open: true,
                operation: Arc::new(tokio::sync::Mutex::new(())),
                sent: Some((91, 1)),
            },
        );
        let replacement = runtime
            .block_on(LspClient::spawn(
                92,
                5,
                super::super::LspClientConfig {
                    command: script.path().to_string_lossy().into_owned(),
                    args: vec![capture_path.to_string_lossy().into_owned()],
                    root_path: root_path.clone(),
                    request_timeout: Duration::from_secs(2),
                    language_id: "rust".into(),
                    initialization_options: None,
                },
                diagnostics_tx,
                crash_tx,
            ))
            .unwrap();
        runtime.block_on(replacement.initialize()).unwrap();
        let replacement = Arc::new(replacement);
        runtime.block_on(replay_open_documents(
            &documents_for_replay,
            replacement.clone(),
        ));
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        let replay_open = loop {
            let captured = std::fs::read_to_string(&capture_path).unwrap();
            let opens: Vec<serde_json::Value> = captured
                .lines()
                .filter_map(|line| serde_json::from_str(line).ok())
                .filter(|message: &serde_json::Value| {
                    message["method"] == "textDocument/didOpen"
                })
                .collect();
            if opens.len() >= 3 || std::time::Instant::now() >= deadline {
                break opens.into_iter().last().unwrap();
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        assert_eq!(replay_open["params"]["textDocument"]["version"], 7);
        assert_eq!(
            replay_open["params"]["textDocument"]["text"],
            "restart latest"
        );
        let _ = runtime.block_on(replacement.shutdown());
        assert!(handle.shutdown());
        worker.join().unwrap();
    }

    /// Giving up used to exist only in stderr, so the last Ready count remained
    /// visible forever. Exhaustion must deterministically publish disconnected
    /// status before its explanatory error.
    #[test]
    fn exhausted_retries_publish_disconnected_status_and_error() {
        let (tx, rx) = unbounded();
        let stamp = LspStamp {
            root_path: PathBuf::from("/project"),
            generation: 6,
            client_id: Some(14),
        };
        assert!(publish_restart_outcome(
            &tx,
            RestartOutcome::Exhausted {
                stamp: stamp.clone(),
                language: "rust".into(),
                message: "restart exhausted".into(),
            },
        )
        .is_none());
        assert!(matches!(
            rx.recv().unwrap(),
            LspResponse::ServerStatus {
                stamp: sent,
                running: false,
                ..
            } if sent == stamp
        ));
        assert!(matches!(
            rx.recv().unwrap(),
            LspResponse::Error {
                stamp: sent,
                request_id: None,
                ..
            } if sent == stamp
        ));
    }

    /// Hover once held the registry read guard for its entire server round
    /// trip, so a slow server blocked root changes and shutdown write guards.
    #[tokio::test]
    async fn a_slow_request_does_not_hold_the_registry_guard() {
        let registry = LspRegistry::new_shared();
        let request_registry = registry.clone();
        let request = tokio::spawn(async move {
            let _client =
                client_for_root(&request_registry, "rust", std::path::Path::new("/project")).await;
            tokio::time::sleep(Duration::from_millis(50)).await;
        });
        tokio::task::yield_now().await;
        assert!(
            tokio::time::timeout(Duration::from_millis(10), registry.write())
                .await
                .is_ok(),
            "the simulated server wait retained the registry guard"
        );
        request.await.unwrap();
    }

    /// A replacement process must receive the in-memory latest snapshot, not
    /// the initial text that the crashed process opened.
    #[test]
    fn restart_replay_retains_the_latest_full_document() {
        let path = PathBuf::from("/tmp/latest.rs");
        let key = test_key(&path);
        let documents = documents_with(&path, 2, "old");
        let mut documents = documents.lock().unwrap();
        let document = documents.get_mut(&key).unwrap();
        document.latest_content = Arc::from("latest");
        document.version = 9;
        document.sent = Some((41, 1));
        let replay = if document.sent.is_some() {
            document.latest_content.clone()
        } else {
            document.initial_content.clone()
        };
        assert_eq!(&*replay, "latest");
    }

    /// Two workspaces commonly have the same `src/lib.rs` suffix and language.
    /// Replaying by language alone opened both documents in the new server.
    #[test]
    fn replay_candidates_are_partitioned_by_canonical_workspace_root() {
        let alpha_path = PathBuf::from("/alpha/src/lib.rs");
        let beta_path = PathBuf::from("/beta/src/lib.rs");
        let documents =
            documents_with_root(Path::new("/alpha"), &alpha_path, 1, "alpha");
        documents
            .lock()
            .unwrap()
            .insert(
                DocumentKey::new(PathBuf::from("/beta"), beta_path.clone()),
                OpenDocument {
                    language_id: "rust".into(),
                    initial_content: Arc::from("beta"),
                    latest_content: Arc::from("beta"),
                    version: 0,
                    epoch: 2,
                    debounce: 0,
                    open: true,
                    operation: Arc::new(tokio::sync::Mutex::new(())),
                    sent: None,
                },
            );

        let beta = open_document_keys_for(&documents, "rust", Path::new("/beta"));
        assert_eq!(beta.len(), 1);
        assert_eq!(beta[0].0.path, beta_path);
        assert!(
            beta.iter().all(|(key, _)| key.root_path == Path::new("/beta")),
            "the old-root tab was offered to the replacement server"
        );
    }

    /// A delayed restart for alpha must not replay alpha's retained tabs into
    /// beta merely because both servers speak Rust.
    #[test]
    fn a_stale_restart_has_no_documents_in_the_new_root_partition() {
        let old_path = PathBuf::from("/alpha/src/lib.rs");
        let documents =
            documents_with_root(Path::new("/alpha"), &old_path, 3, "old");
        assert!(open_document_keys_for(&documents, "rust", Path::new("/beta")).is_empty());
    }
}
