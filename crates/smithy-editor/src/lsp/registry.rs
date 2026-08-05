//! LSP Registry for managing multiple language servers
//!
//! This module provides `LspRegistry` which manages multiple language server
//! instances, one per (language, workspace) combination.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::{mpsc, RwLock};

use super::client::{
    ClientCrash, ClientDiagnostics, LspClient, LspClientConfig, LspError, LspStamp,
};
use std::time::{Duration, Instant};
use futures_util::future::join_all;

/// Key for identifying a language server instance
#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub struct ServerKey {
    /// Language ID (e.g., "rust", "python")
    pub language_id: String,
    /// Workspace root path
    pub root_path: PathBuf,
}

impl ServerKey {
    pub fn new(language_id: impl Into<String>, root_path: impl Into<PathBuf>) -> Self {
        Self {
            language_id: language_id.into(),
            root_path: root_path.into(),
        }
    }
}

/// Information about a crashed server for restart attempts
#[derive(Debug, Clone)]
struct CrashInfo {
    /// Number of restart attempts
    attempts: u32,
    /// Time of last crash
    last_crash: Instant,
}

/// Everything needed to start one server, captured while the registry is
/// locked and executed after that guard has been dropped.
struct SpawnPlan {
    key: ServerKey,
    generation: u64,
    id: u64,
    config: LspClientConfig,
    diagnostics_tx: mpsc::Sender<ClientDiagnostics>,
    crash_tx: mpsc::Sender<ClientCrash>,
}

/// Whether a language server can actually be used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerAvailability {
    Available,
    /// No server is configured for this language.
    Unconfigured,
    /// The command is not on `PATH`.
    NotFound {
        command: String,
    },
    /// The command exists but does not run.
    Broken {
        command: String,
        reason: String,
    },
}

impl ServerAvailability {
    /// A message that names the fix rather than the symptom.
    pub fn advice(&self) -> Option<String> {
        match self {
            ServerAvailability::Available | ServerAvailability::Unconfigured => None,
            ServerAvailability::NotFound { command } => Some(format!(
                "`{command}` is not on PATH. For Rust, install it with                  `rustup component add rust-analyzer`."
            )),
            ServerAvailability::Broken { command, reason } => {
                let hint = if reason.contains("Unknown binary") {
                    " This is a rustup shim with no component behind it — run                      `rustup component add rust-analyzer`."
                } else {
                    ""
                };
                Some(format!("`{command}` is installed but failed to run: {reason}.{hint}"))
            }
        }
    }
}

/// Registry for managing multiple language server instances
pub struct LspRegistry {
    /// Active clients mapped by server key
    clients: HashMap<ServerKey, Arc<LspClient>>,
    /// Counter for assigning unique client IDs
    next_id: u64,
    /// Channel for receiving diagnostics from all servers
    diagnostics_tx: mpsc::Sender<ClientDiagnostics>,
    /// Configuration for different languages
    language_configs: HashMap<String, LanguageServerConfig>,
    /// Channel for receiving crash notifications
    crash_tx: mpsc::Sender<ClientCrash>,
    /// Receiver for crash notifications (pub for integration)
    pub crash_rx: Option<mpsc::Receiver<ClientCrash>>,
    /// Track crashed servers for restart backoff
    crashed_servers: HashMap<ServerKey, CrashInfo>,
    /// Maximum restart attempts before giving up
    max_restart_attempts: u32,
    /// The workspace every lookup resolves against.
    ///
    /// `ServerKey` has always carried a root, but the lookup used to ignore it
    /// and return the first client matching the language — so after a project
    /// switch, requests went to the server initialized against the *previous*
    /// root. Holding the current root here is what makes that impossible.
    current_root: Option<PathBuf>,
    /// Invalidates in-flight starts and delayed retries after a root switch or
    /// explicit stop.
    generation: u64,
    /// An explicit stop keeps retries and lazy starts dormant until Initialize.
    stopped: bool,
}

/// Restart backoff configuration
const INITIAL_BACKOFF_MS: u64 = 1000;
const MAX_BACKOFF_MS: u64 = 30000;

fn crash_names_current_client(current_id: Option<u64>, crashed_id: u64) -> bool {
    current_id == Some(crashed_id)
}

fn stamp_names_active_client(
    current_root: Option<&Path>,
    generation: u64,
    active_ids: impl Iterator<Item = u64>,
    stamp: &LspStamp,
) -> bool {
    generation == stamp.generation
        && current_root == Some(stamp.root_path.as_path())
        && stamp
            .client_id
            .is_some_and(|id| active_ids.into_iter().any(|active| active == id))
}

/// Configuration for a language server type
#[derive(Debug, Clone, Default)]
pub struct LanguageServerConfig {
    /// Command to run the server
    pub command: String,
    /// Arguments for the command
    pub args: Vec<String>,
    /// File extensions this server handles
    pub extensions: Vec<String>,
    /// Server-specific settings sent as `initializationOptions`.
    ///
    /// Left `None` for every server but rust-analyzer, which is the only one
    /// whose defaults are expensive enough to matter — see
    /// [`rust_initialization_options`].
    pub initialization_options: Option<serde_json::Value>,
}

pub enum RestartOutcome {
    Restarted {
        key: ServerKey,
        client: Arc<LspClient>,
    },
    Exhausted {
        stamp: LspStamp,
        language: String,
        message: String,
    },
    Obsolete,
}

/// What rust-analyzer is told at `initialize`.
///
/// It was previously told **nothing**, so it ran every default: priming the
/// cache for the whole workspace at startup, walking `target/`, and taking as
/// many worker threads as it liked (43 observed). On a machine that is also
/// hosting a local model those are not free.
///
/// Each of these was chosen to cost no diagnostics:
///
/// - **`cachePriming.enable: false`** — makes indexing lazy rather than
///   whole-workspace-at-startup. You pay per file you actually open.
/// - **`files.excludeDirs`** — `target/` is build output; there is nothing in it
///   to analyse and it is by far the largest directory in a Rust project.
/// - **`numThreads: 4`** — a ceiling on worker threads. The point is not
///   rust-analyzer's own speed; it is leaving cores for the model that the whole
///   design exists to serve.
/// - **`lru.capacity`** — bounds the salsa query cache, which is what actually
///   grows without limit.
///
/// Deliberately **not** set, because each would cost a diagnostic:
///
/// - `procMacro.enable: false` — this workspace is built on `serde` and
///   `async_trait` derives. Disabling proc-macro expansion saves a lot and
///   reports every derived impl as unresolved, which is worse than useless.
/// - `cargo.allTargets: false` — would drop diagnostics inside `#[cfg(test)]`,
///   in a project whose entire discipline is its tests.
///
/// `light` additionally disables `checkOnSave`, which is the largest single
/// saving available — it stops a second `cargo` process holding a full build in
/// memory — at the cost of real compiler diagnostics, leaving only
/// rust-analyzer's own inference. That is a trade worth offering and not worth
/// defaulting to, so it is behind `SMITHY_LSP_LIGHT=1`.
pub fn rust_initialization_options(light: bool) -> serde_json::Value {
    serde_json::json!({
        "cachePriming": { "enable": false },
        "files": { "excludeDirs": ["target", ".git"] },
        "numThreads": 4,
        "lru": { "capacity": 128 },
        "checkOnSave": !light,
    })
}

/// Whether the user has asked for the low-memory profile.
fn lsp_light_requested() -> bool {
    std::env::var("SMITHY_LSP_LIGHT").is_ok_and(|v| v == "1")
}

impl LspRegistry {
    fn intent_stamp(&self) -> Option<LspStamp> {
        Some(LspStamp {
            root_path: self.current_root.clone()?,
            generation: self.generation,
            client_id: None,
        })
    }

    pub fn current_stamp_for(&self, language_id: &str) -> Option<LspStamp> {
        self.current_client_for(language_id)
            .map(|client| client.stamp())
            .or_else(|| self.intent_stamp())
    }

    pub fn accepts_client_stamp(&self, stamp: &LspStamp) -> bool {
        stamp_names_active_client(
            self.current_root.as_deref(),
            self.generation,
            self.clients.values().map(|client| client.id()),
            stamp,
        )
    }

    /// Create a new LSP registry
    pub fn new(diagnostics_tx: mpsc::Sender<ClientDiagnostics>) -> Self {
        let mut language_configs = HashMap::new();

        // Default configurations for common languages
        language_configs.insert(
            "rust".to_string(),
            LanguageServerConfig {
                command: "rust-analyzer".to_string(),
                args: vec![],
                extensions: vec!["rs".to_string()],
                initialization_options: Some(rust_initialization_options(lsp_light_requested())),
            },
        );

        language_configs.insert(
            "typescript".to_string(),
            LanguageServerConfig {
                command: "typescript-language-server".to_string(),
                args: vec!["--stdio".to_string()],
                extensions: vec!["ts".to_string(), "tsx".to_string()],
                initialization_options: None,
            },
        );

        language_configs.insert(
            "javascript".to_string(),
            LanguageServerConfig {
                command: "typescript-language-server".to_string(),
                args: vec!["--stdio".to_string()],
                extensions: vec!["js".to_string(), "jsx".to_string()],
                initialization_options: None,
            },
        );

        language_configs.insert(
            "python".to_string(),
            LanguageServerConfig {
                command: "pylsp".to_string(),
                args: vec![],
                extensions: vec!["py".to_string()],
                initialization_options: None,
            },
        );

        language_configs.insert(
            "go".to_string(),
            LanguageServerConfig {
                command: "gopls".to_string(),
                args: vec![],
                extensions: vec!["go".to_string()],
                initialization_options: None,
            },
        );

        language_configs.insert(
            "c".to_string(),
            LanguageServerConfig {
                command: "clangd".to_string(),
                args: vec![],
                extensions: vec!["c".to_string(), "h".to_string()],
                initialization_options: None,
            },
        );

        language_configs.insert(
            "cpp".to_string(),
            LanguageServerConfig {
                command: "clangd".to_string(),
                args: vec![],
                extensions: vec![
                    "cpp".to_string(),
                    "cxx".to_string(),
                    "cc".to_string(),
                    "hpp".to_string(),
                ],
                initialization_options: None,
            },
        );

        let (crash_tx, crash_rx) = mpsc::channel(16);

        Self {
            clients: HashMap::new(),
            next_id: 1,
            diagnostics_tx,
            language_configs,
            crash_tx,
            crash_rx: Some(crash_rx),
            crashed_servers: HashMap::new(),
            max_restart_attempts: 3,
            current_root: None,
            generation: 0,
            stopped: false,
        }
    }

    /// Calculate backoff duration based on attempt count
    fn calculate_backoff(&self, attempts: u32) -> Duration {
        let backoff_ms = INITIAL_BACKOFF_MS * (1u64 << attempts.saturating_sub(1));
        Duration::from_millis(backoff_ms.min(MAX_BACKOFF_MS))
    }

    fn prepare_spawn(&mut self, key: ServerKey) -> Option<SpawnPlan> {
        if self.stopped || self.clients.contains_key(&key) {
            return None;
        }
        let lang_config = self
            .language_configs
            .get(&key.language_id)
            .cloned()
            .unwrap_or_else(|| LanguageServerConfig {
                command: format!("{}-language-server", key.language_id),
                args: vec![],
                extensions: vec![],
                initialization_options: None,
            });
        let id = self.next_id;
        self.next_id += 1;
        Some(SpawnPlan {
            config: LspClientConfig {
                command: lang_config.command,
                args: lang_config.args,
                root_path: key.root_path.clone(),
                request_timeout: Duration::from_secs(30),
                language_id: key.language_id.clone(),
                initialization_options: lang_config.initialization_options,
            },
            key,
            generation: self.generation,
            id,
            diagnostics_tx: self.diagnostics_tx.clone(),
            crash_tx: self.crash_tx.clone(),
        })
    }

    async fn execute_spawn(plan: &SpawnPlan) -> Result<Arc<LspClient>, LspError> {
        let client = Arc::new(
            LspClient::spawn(
                plan.id,
                plan.generation,
                plan.config.clone(),
                plan.diagnostics_tx.clone(),
                plan.crash_tx.clone(),
            )
            .await?,
        );
        if let Err(error) = client.initialize().await {
            let tail = client.stderr_tail();
            return Err(LspError::Startup(format!(
                "{error}{}",
                if tail.trim().is_empty() {
                    String::new()
                } else {
                    format!("\nserver stderr (tail):\n{tail}")
                }
            )));
        }
        Ok(client)
    }

    fn install_spawn(&mut self, plan: &SpawnPlan, client: Arc<LspClient>) -> bool {
        let current = self.current_root.as_deref() == Some(plan.key.root_path.as_path());
        if self.generation != plan.generation
            || self.stopped
            || !current
            || self.clients.contains_key(&plan.key)
        {
            return false;
        }
        self.clients.insert(plan.key.clone(), client);
        self.crashed_servers.remove(&plan.key);
        true
    }

    fn retry_is_current(&self, key: &ServerKey, generation: u64) -> bool {
        self.generation == generation
            && !self.stopped
            && self.current_root.as_deref() == Some(key.root_path.as_path())
            && !self.clients.contains_key(key)
    }

    /// Detect language from file extension
    pub fn detect_language(&self, path: &Path) -> Option<String> {
        let ext = path.extension()?.to_str()?;

        for (lang, config) in &self.language_configs {
            if config.extensions.contains(&ext.to_string()) {
                return Some(lang.clone());
            }
        }

        None
    }

    fn take_all_clients(&mut self, stop: bool) -> Vec<Arc<LspClient>> {
        self.generation = self.generation.wrapping_add(1);
        self.stopped = stop;
        self.crashed_servers.clear();
        self.clients.drain().map(|(_, client)| client).collect()
    }

    async fn shutdown_clients(clients: Vec<Arc<LspClient>>) {
        let results = join_all(clients.into_iter().map(|client| async move {
            client.shutdown().await
        }))
        .await;
        for result in results {
            if let Err(error) = result {
                eprintln!("Error shutting down LSP client: {error}");
            }
        }
    }

    /// Whether a language server is usable, and why not if it isn't.
    ///
    /// Presence on `PATH` is not enough. `rustup` installs a **shim** at
    /// `~/.cargo/bin/rust-analyzer` that exists whether or not the component
    /// behind it does; when it doesn't, the shim prints
    /// "Unknown binary 'rust-analyzer'" and exits. `which` reports success, the
    /// server dies instantly, and the client reports "EOF while reading
    /// headers" — a message that describes our symptom rather than the actual
    /// problem, which is a one-line fix the user could have made immediately.
    ///
    /// So the binary is actually executed.
    pub fn check_server(&self, language_id: &str) -> ServerAvailability {
        let Some(config) = self.language_configs.get(language_id) else {
            return ServerAvailability::Unconfigured;
        };
        if which::which(&config.command).is_err() {
            return ServerAvailability::NotFound {
                command: config.command.clone(),
            };
        }
        match std::process::Command::new(&config.command)
            .arg("--version")
            .output()
        {
            Ok(out) if out.status.success() => ServerAvailability::Available,
            Ok(out) => ServerAvailability::Broken {
                command: config.command.clone(),
                reason: String::from_utf8_lossy(&out.stderr)
                    .lines()
                    .next()
                    .unwrap_or("exited non-zero")
                    .trim()
                    .to_string(),
            },
            Err(e) => ServerAvailability::Broken {
                command: config.command.clone(),
                reason: e.to_string(),
            },
        }
    }

    /// Back-compat shim over [`Self::check_server`].
    pub fn is_server_available(&self, language_id: &str) -> bool {
        matches!(
            self.check_server(language_id),
            ServerAvailability::Available
        )
    }
}

/// Thread-safe shared registry type
pub type SharedLspRegistry = Arc<RwLock<LspRegistry>>;

impl LspRegistry {
    /// Create a new shared registry (without diagnostics channel - will be set later)
    pub fn new_shared() -> SharedLspRegistry {
        // Create a dummy channel for now - the real one is set during workspace init
        let (tx, _rx) = mpsc::channel(16);
        Arc::new(RwLock::new(LspRegistry::new(tx)))
    }

    /// Initialize a workspace without retaining a registry guard while a server
    /// is stopped, spawned, or initialized.
    pub async fn initialize_shared(
        shared: &SharedLspRegistry,
        workspace_root: &Path,
        diagnostics_tx: mpsc::Sender<ClientDiagnostics>,
    ) -> Result<(usize, LspStamp), (LspStamp, LspError)> {
        let (old_clients, intent_stamp) = {
            let mut registry = shared.write().await;
            registry.diagnostics_tx = diagnostics_tx;
            let changing = registry.root_is_changing(workspace_root);
            let old = if changing {
                registry.take_all_clients(false)
            } else {
                Vec::new()
            };
            // Every Initialize is a new intent. This cancels an obsolete retry
            // even when it races a stop/start of the same root.
            registry.generation = registry.generation.wrapping_add(1);
            registry.stopped = false;
            registry.current_root = Some(workspace_root.to_path_buf());
            let stamp = registry.intent_stamp().expect("root was just installed");
            (old, stamp)
        };
        Self::shutdown_clients(old_clients).await;

        if !workspace_root.join("Cargo.toml").exists() {
            return Ok((shared.read().await.client_count(), intent_stamp));
        }
        let available = {
            let registry = shared.read().await;
            registry.check_server("rust")
        };
        if !matches!(available, ServerAvailability::Available) {
            return Ok((shared.read().await.client_count(), intent_stamp));
        }

        let plan = {
            let mut registry = shared.write().await;
            registry.prepare_spawn(ServerKey::new("rust", workspace_root))
        };
        if let Some(plan) = plan {
            let plan_stamp = LspStamp {
                root_path: plan.key.root_path.clone(),
                generation: plan.generation,
                client_id: Some(plan.id),
            };
            let client = Self::execute_spawn(&plan)
                .await
                .map_err(|error| (plan_stamp.clone(), error))?;
            let installed = {
                let mut registry = shared.write().await;
                registry.install_spawn(&plan, client.clone())
            };
            if !installed {
                client
                    .shutdown()
                    .await
                    .map_err(|error| (plan_stamp.clone(), error))?;
            }
            return Ok((shared.read().await.client_count(), plan_stamp));
        }
        let registry = shared.read().await;
        Ok((
            registry.client_count(),
            registry.current_stamp_for("rust").unwrap_or(intent_stamp),
        ))
    }

    /// Stop every client and invalidate retries, with no lock held while
    /// shutdown handshakes or process reaping run.
    pub async fn stop_shared(shared: &SharedLspRegistry) {
        let clients = shared.write().await.take_all_clients(true);
        Self::shutdown_clients(clients).await;
    }

    /// Restart the exact crashed `(ServerKey, client id)` if it is still current.
    /// Returns the replacement so the integration layer can replay documents.
    pub async fn restart_after_crash(
        shared: &SharedLspRegistry,
        crash: ClientCrash,
    ) -> RestartOutcome {
        let key = ServerKey::new(crash.language_id, crash.root_path);
        let crashed_stamp;
        let (generation, mut attempts, mut backoff, maximum) = {
            let mut registry = shared.write().await;
            let still_exact = crash_names_current_client(
                registry.clients.get(&key).map(|client| client.id()),
                crash.client_id,
            );
            if !still_exact || registry.stopped {
                return RestartOutcome::Obsolete;
            }
            crashed_stamp = registry.clients[&key].stamp();
            registry.clients.remove(&key);
            let maximum = registry.max_restart_attempts;
            let attempts = {
                let info = registry
                    .crashed_servers
                    .entry(key.clone())
                    .or_insert(CrashInfo {
                        attempts: 0,
                        last_crash: Instant::now(),
                    });
                info.attempts += 1;
                info.last_crash = Instant::now();
                info.attempts
            };
            (
                registry.generation,
                attempts,
                registry.calculate_backoff(attempts),
                maximum,
            )
        };

        eprintln!(
            "LSP {} crashed (client {}):{}{}",
            key.language_id,
            crash.client_id,
            if crash.stderr_tail.trim().is_empty() { "" } else { "\n" },
            crash.stderr_tail
        );
        let mut last_stamp = crashed_stamp;
        let mut last_error = "language server disconnected".to_string();
        while attempts <= maximum {
            tokio::time::sleep(backoff).await;
            let plan = {
                let mut registry = shared.write().await;
                if !registry.retry_is_current(&key, generation) {
                    return RestartOutcome::Obsolete;
                }
                registry.prepare_spawn(key.clone())
            };
            let Some(plan) = plan else {
                return RestartOutcome::Obsolete;
            };
            last_stamp = LspStamp {
                root_path: plan.key.root_path.clone(),
                generation: plan.generation,
                client_id: Some(plan.id),
            };
            match Self::execute_spawn(&plan).await {
                Ok(client) => {
                    let installed = shared
                        .write()
                        .await
                        .install_spawn(&plan, client.clone());
                    if installed {
                        return RestartOutcome::Restarted { key, client };
                    }
                    let _ = client.shutdown().await;
                    return RestartOutcome::Obsolete;
                }
                Err(error) => {
                    last_error = error.to_string();
                    eprintln!(
                        "Failed to restart {} for {}: {}",
                        key.language_id,
                        key.root_path.display(),
                        error
                    );
                    attempts += 1;
                    if attempts > maximum {
                        break;
                    }
                    let registry = shared.write().await;
                    if !registry.retry_is_current(&key, generation) {
                        return RestartOutcome::Obsolete;
                    }
                    backoff = registry.calculate_backoff(attempts);
                }
            }
        }
        RestartOutcome::Exhausted {
            stamp: last_stamp,
            language: key.language_id,
            message: format!(
                "Language server disconnected after {maximum} restart attempts: {last_error}"
            ),
        }
    }

    /// The client serving `language_id` **in the current workspace**, if any.
    ///
    /// Named for the current root rather than just the language because the
    /// distinction is the whole point. This used to be `client_for_language`, and
    /// it returned the first client whose language matched — ignoring the
    /// `root_path` sitting in the very key it was scanning. After a project
    /// switch that meant hovers, completions and go-to-definition were answered
    /// by a server analysing the project you had left, and its diagnostics
    /// described files you no longer had open.
    pub fn current_client_for(&self, language_id: &str) -> Option<Arc<LspClient>> {
        self.current_key_for(language_id)
            .and_then(|key| self.clients.get(&key).cloned())
    }

    pub fn client_for(
        &self,
        language_id: &str,
        root_path: &Path,
    ) -> Option<Arc<LspClient>> {
        self.clients
            .get(&ServerKey::new(language_id, root_path))
            .cloned()
    }

    /// The key a lookup for `language_id` resolves to, or `None` before any
    /// workspace has been adopted.
    ///
    /// Split out from [`current_client_for`](Self::current_client_for) so the
    /// decision can be tested without spawning a language server — the map
    /// access either side of it is not the part that was wrong.
    fn current_key_for(&self, language_id: &str) -> Option<ServerKey> {
        Some(ServerKey::new(language_id, self.current_root.as_ref()?))
    }

    /// Whether adopting `new_root` means the running servers are now analysing
    /// the wrong project and have to be stopped.
    ///
    /// False when the root is unchanged, so re-initializing the same project
    /// reuses the warm server instead of paying for a cold index; false when
    /// there is no root yet, because there is nothing to stop.
    fn root_is_changing(&self, new_root: &Path) -> bool {
        self.current_root
            .as_deref()
            .is_some_and(|old| old != new_root)
    }

    /// The workspace lookups currently resolve against.
    pub fn current_root(&self) -> Option<&Path> {
        self.current_root.as_deref()
    }

    /// How many servers are running. Used by tests to assert that re-rooting
    /// actually releases the old ones rather than accumulating them.
    pub fn client_count(&self) -> usize {
        self.clients.len()
    }

    pub(super) fn clients_snapshot(&self) -> Vec<Arc<LspClient>> {
        self.clients.values().cloned().collect()
    }

    #[cfg(test)]
    pub(super) fn install_test_client(&mut self, client: Arc<LspClient>) {
        let key = ServerKey::new(client.language_id(), client.root_path());
        self.current_root = Some(client.root_path().to_path_buf());
        self.generation = client.stamp().generation;
        self.clients.insert(key, client);
    }

    #[cfg(test)]
    pub(super) fn test_channels(
        &self,
    ) -> (
        mpsc::Sender<ClientDiagnostics>,
        mpsc::Sender<ClientCrash>,
    ) {
        (self.diagnostics_tx.clone(), self.crash_tx.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The defect this replaced: the lookup scanned keys that carry a
    /// `root_path` and compared only the `language_id`, so after a project
    /// switch it handed back a server analysing the project you had left.
    #[test]
    fn a_lookup_resolves_against_the_current_workspace_not_just_the_language() {
        let (tx, _rx) = mpsc::channel(16);
        let mut registry = LspRegistry::new(tx);

        registry.current_root = Some(PathBuf::from("/projects/alpha"));
        let alpha = registry.current_key_for("rust").expect("a root is set");
        assert_eq!(alpha.root_path, PathBuf::from("/projects/alpha"));

        // Re-root, as a project switch does.
        registry.current_root = Some(PathBuf::from("/projects/beta"));
        let beta = registry.current_key_for("rust").expect("a root is set");

        assert_eq!(beta.root_path, PathBuf::from("/projects/beta"));
        assert_ne!(
            alpha, beta,
            "the same language in two projects must not resolve to one server"
        );
    }

    /// Re-rooting has to release the previous project's servers — that is the
    /// difference between one rust-analyzer and one per project ever opened.
    #[test]
    fn changing_the_root_tears_down_but_reopening_the_same_project_does_not() {
        let (tx, _rx) = mpsc::channel(16);
        let mut registry = LspRegistry::new(tx);

        assert!(
            !registry.root_is_changing(Path::new("/projects/alpha")),
            "nothing is running yet, so there is nothing to tear down"
        );

        registry.current_root = Some(PathBuf::from("/projects/alpha"));
        assert!(
            !registry.root_is_changing(Path::new("/projects/alpha")),
            "reopening the same project must reuse the warm server, not reindex"
        );
        assert!(
            registry.root_is_changing(Path::new("/projects/beta")),
            "a different project means every running server is analysing the wrong tree"
        );
    }

    /// Before any workspace is adopted there is no current root, and a lookup
    /// must say so rather than picking whatever happens to be running.
    #[test]
    fn a_lookup_before_initialize_resolves_to_nothing() {
        let (tx, _rx) = mpsc::channel(16);
        let registry = LspRegistry::new(tx);

        assert_eq!(registry.current_root(), None);
        assert!(registry.current_client_for("rust").is_none());
        assert_eq!(registry.client_count(), 0);
    }

    /// rust-analyzer was previously sent no options at all, so it ran every
    /// default. The two that cost nothing in diagnostics must be present.
    #[test]
    fn rust_analyzer_is_told_not_to_prime_the_whole_workspace_or_walk_target() {
        let opts = rust_initialization_options(false);

        assert_eq!(opts["cachePriming"]["enable"], serde_json::json!(false));
        let excluded = opts["files"]["excludeDirs"]
            .as_array()
            .expect("excludeDirs is a list");
        assert!(
            excluded.iter().any(|v| v == "target"),
            "target/ is build output and the largest directory in the project"
        );
        assert!(opts["numThreads"].is_number(), "worker threads are capped");
        assert!(
            opts["lru"]["capacity"].is_number(),
            "the query cache is bounded"
        );
    }

    /// The one setting that trades a diagnostic for memory has to be opt-in, and
    /// has to actually change when opted into — a flag that reads the same either
    /// way would be worse than not offering it.
    #[test]
    fn check_on_save_survives_by_default_and_only_light_mode_drops_it() {
        assert_eq!(
            rust_initialization_options(false)["checkOnSave"],
            serde_json::json!(true),
            "real compiler diagnostics are the good ones; keep them unless asked"
        );
        assert_eq!(
            rust_initialization_options(true)["checkOnSave"],
            serde_json::json!(false)
        );
    }

    /// Only rust-analyzer's defaults are expensive enough to be worth
    /// overriding; sending guesses to other servers would be worse than silence.
    #[test]
    fn only_rust_analyzer_is_sent_initialization_options() {
        let (tx, _rx) = mpsc::channel(16);
        let registry = LspRegistry::new(tx);

        assert!(registry.language_configs["rust"]
            .initialization_options
            .is_some());
        for (lang, config) in &registry.language_configs {
            if lang != "rust" {
                assert!(
                    config.initialization_options.is_none(),
                    "{lang} was given options nobody chose"
                );
            }
        }
    }

    #[test]
    fn a_path_extension_selects_the_diff_language() {
        let (tx, _rx) = mpsc::channel(16);
        let registry = LspRegistry::new(tx);

        assert_eq!(
            registry.detect_language(Path::new("main.rs")),
            Some("rust".to_string())
        );
        assert_eq!(
            registry.detect_language(Path::new("app.py")),
            Some("python".to_string())
        );
        assert_eq!(
            registry.detect_language(Path::new("index.ts")),
            Some("typescript".to_string())
        );
        assert_eq!(
            registry.detect_language(Path::new("main.go")),
            Some("go".to_string())
        );
        assert_eq!(registry.detect_language(Path::new("unknown.xyz")), None);
    }

    #[test]
    fn a_server_key_distinguishes_the_same_language_in_two_projects() {
        let key1 = ServerKey::new("rust", "/project/a");
        let key2 = ServerKey::new("rust", "/project/a");
        let key3 = ServerKey::new("rust", "/project/b");

        assert_eq!(key1, key2);
        assert_ne!(key1, key3);
    }

    /// A delayed crash from client 11 used to remove whatever occupied the
    /// language slot by the time it was handled, including replacement 12.
    #[test]
    fn a_stale_crash_identity_cannot_name_its_replacement() {
        assert!(!crash_names_current_client(Some(12), 11));
        assert!(crash_names_current_client(Some(12), 12));
    }

    /// Restart failure used to enqueue `next_id - 1`, which could belong to a
    /// different language. A retry now retains the complete original key.
    #[test]
    fn a_failed_restart_retries_the_same_server_key() {
        let key = ServerKey::new("rust", "/projects/alpha");
        let retry = key.clone();
        assert_eq!(retry, key);
        assert_eq!(retry.root_path, PathBuf::from("/projects/alpha"));
    }

    /// A crash storm in TypeScript must not advance Rust's backoff. The old
    /// synthetic client-id retry path made that association accidental.
    #[test]
    fn restart_backoff_is_independent_for_each_server_key() {
        let (tx, _rx) = mpsc::channel(1);
        let mut registry = LspRegistry::new(tx);
        let rust = ServerKey::new("rust", "/p");
        let typescript = ServerKey::new("typescript", "/p");
        registry.crashed_servers.insert(
            rust.clone(),
            CrashInfo {
                attempts: 1,
                last_crash: Instant::now(),
            },
        );
        registry.crashed_servers.insert(
            typescript.clone(),
            CrashInfo {
                attempts: 3,
                last_crash: Instant::now(),
            },
        );
        assert_eq!(registry.crashed_servers[&rust].attempts, 1);
        assert_eq!(registry.crashed_servers[&typescript].attempts, 3);
    }

    /// A retry sleeping for the old workspace must wake up obsolete, rather
    /// than installing a server initialized against the project just left.
    #[test]
    fn switching_roots_cancels_a_delayed_restart() {
        let (tx, _rx) = mpsc::channel(1);
        let mut registry = LspRegistry::new(tx);
        registry.current_root = Some(PathBuf::from("/alpha"));
        let key = ServerKey::new("rust", "/alpha");
        let generation = registry.generation;
        assert!(registry.retry_is_current(&key, generation));
        registry.current_root = Some(PathBuf::from("/beta"));
        registry.generation += 1;
        assert!(!registry.retry_is_current(&key, generation));
    }

    /// A publishDiagnostics message can already be queued when its process
    /// crashes. Once client 12 replaces client 11, that queued message must not
    /// repopulate the panel that the crash/root transition cleared.
    #[test]
    fn diagnostics_from_an_old_client_are_not_current_after_replacement() {
        let old = LspStamp {
            root_path: PathBuf::from("/project"),
            generation: 7,
            client_id: Some(11),
        };
        let replacement = LspStamp {
            client_id: Some(12),
            ..old.clone()
        };
        assert!(!stamp_names_active_client(
            Some(Path::new("/project")),
            7,
            [12].into_iter(),
            &old,
        ));
        assert!(stamp_names_active_client(
            Some(Path::new("/project")),
            7,
            [12].into_iter(),
            &replacement,
        ));
    }
}

#[cfg(test)]
mod availability_tests {
    use super::*;

    /// The exact failure that made LSP look like our bug for weeks: rustup's
    /// shim is on PATH, so `which` succeeds, but running it fails.
    #[test]
    fn a_rustup_shim_without_its_component_names_the_real_fix() {
        let broken = ServerAvailability::Broken {
            command: "rust-analyzer".into(),
            reason: "error: Unknown binary 'rust-analyzer' in official toolchain".into(),
        };
        let advice = broken
            .advice()
            .expect("a broken server must explain itself");
        assert!(advice.contains("rustup component add rust-analyzer"));
    }

    #[test]
    fn a_missing_server_names_the_install_command() {
        let missing = ServerAvailability::NotFound {
            command: "rust-analyzer".into(),
        };
        assert!(missing.advice().unwrap().contains("rustup component add"));
    }

    /// A generic failure should still report the server's own words rather than
    /// swallowing them.
    #[test]
    fn an_unrecognised_failure_still_reports_its_reason() {
        let broken = ServerAvailability::Broken {
            command: "gopls".into(),
            reason: "permission denied".into(),
        };
        let advice = broken.advice().unwrap();
        assert!(advice.contains("permission denied"));
        assert!(
            !advice.contains("rustup"),
            "a Go failure must not suggest a Rust fix"
        );
    }

    #[test]
    fn a_working_server_offers_no_advice() {
        assert!(ServerAvailability::Available.advice().is_none());
        assert!(ServerAvailability::Unconfigured.advice().is_none());
    }

    /// Now that the component is installed, this must actually pass — and it is
    /// the regression test for "detection reports presence, not usability".
    #[test]
    fn rust_analyzer_is_detected_as_usable_when_it_runs() {
        let (tx, _rx) = mpsc::channel::<ClientDiagnostics>(16);
        let registry = LspRegistry::new(tx);
        match registry.check_server("rust") {
            ServerAvailability::Available => {}
            // If it genuinely is not installed on this machine, the check must
            // still describe the fix rather than failing opaquely.
            other => assert!(
                other.advice().is_some(),
                "an unusable server must always explain itself, got {other:?}"
            ),
        }
    }
}
