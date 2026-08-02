//! Application state.
//!
//! forge kept three overlapping copies of the same state — `AppState`,
//! `BufferManager`, and a pile of loose signals in `main.rs` — and its own
//! README named that as the top architectural problem. This is one layer:
//! non-reactive state in [`AppState`], reactive signals in [`AppSignals`], and
//! the agent's own state inside [`AgentPanelState`].

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use crossbeam_channel::{unbounded, Receiver, Sender};
use floem::ext_event::update_signal_from_channel;
use floem::reactive::{RwSignal, SignalGet, SignalUpdate};

use smithy_agent::{Session, TurnEvent};
use smithy_editor::{
    AgentPanelState, BufferManager, BufferState, DiagnosticsState, FileBrowserState, LayoutTheme,
    LspHandle, LspManager, LspResponse, PendingChangeManager, TerminalTabManager,
};

use crate::runtime::tokio_runtime;

/// A shell command waiting for the user's go-ahead.
///
/// The responder is `Arc<Mutex<Option<..>>>` because the request travels through
/// a reactive signal, which requires `Clone`, while a oneshot sender is
/// single-use.
#[derive(Clone)]
pub struct ShellApprovalRequest {
    pub command: String,
    pub responder: Arc<Mutex<Option<tokio::sync::oneshot::Sender<bool>>>>,
}

impl ShellApprovalRequest {
    /// Answer the request. Safe to call more than once; only the first wins.
    pub fn respond(&self, approve: bool) {
        if let Ok(mut slot) = self.responder.lock() {
            if let Some(tx) = slot.take() {
                let _ = tx.send(approve);
            }
        }
    }
}

/// Everything the agent task sends back to the UI thread.
#[derive(Clone)]
pub enum AgentUiEvent {
    /// The session connected; carries the model label and derived budget.
    Ready {
        model_label: String,
        context_limit: i64,
        context_summary: String,
        /// Transcript of a resumed conversation; empty for a fresh session.
        restored: Vec<smithy_agent::TranscriptEntry>,
        /// Set when resuming, so saves append to the same file.
        resumed_id: Option<String>,
    },
    /// The session could not start.
    Unavailable(String),
    /// Progress from inside a turn.
    Turn(TurnEvent),
    /// A write is queued for review.
    ReviewRequested(smithy_editor::PendingFileChange),
    Answered(String),
    Stopped(String),
    Failed(String),
}

/// Messages that have arrived from a worker thread and not yet been handled.
pub type Inbox<T> = Arc<Mutex<std::collections::VecDeque<T>>>;

/// Language-server messages awaiting the UI thread.
pub type LspInbox = Inbox<LspResponse>;

/// Bridge a channel onto the UI thread **without losing messages**.
///
/// `update_signal_from_channel` drains its whole queue inside a single effect run,
/// calling `set` once per message; downstream effects do not run in between, so
/// they observe only the last value. A signal holds a value, not a stream.
///
/// This was not theoretical. rust-analyzer publishes diagnostics one notification
/// per file, so a batch covering five files arrived as five `set` calls and one
/// effect run — four files silently discarded. Every payload-carrying bridge in the
/// app had the same defect.
///
/// So the bridge carries a bare `()` tick, where coalescing is harmless, and the
/// payloads wait in a queue the effect drains itself. Same shape the terminal's
/// 60fps poll has always used.
pub fn bridge<T: Send + 'static>(rx: Receiver<T>) -> (RwSignal<Option<()>>, Inbox<T>) {
    let inbox: Inbox<T> = Arc::new(Mutex::new(std::collections::VecDeque::new()));
    let tick = RwSignal::new(None::<()>);

    let (tick_tx, tick_rx) = unbounded::<()>();
    update_signal_from_channel(tick.write_only(), tick_rx);

    let queue = inbox.clone();
    std::thread::spawn(move || {
        while let Ok(item) = rx.recv() {
            if let Ok(mut queue) = queue.lock() {
                queue.push_back(item);
            }
            // A failed send means the UI is gone; nothing left to notify.
            if tick_tx.send(()).is_err() {
                break;
            }
        }
    });

    (tick, inbox)
}

/// Take the next item, if there is one.
///
/// For a queue whose consumer can only show one thing at a time — the shell
/// approval modal — where draining would mean discarding every request but the
/// last, and each discarded request denies a command the user never saw.
pub fn pop<T>(inbox: &Inbox<T>) -> Option<T> {
    inbox.lock().ok()?.pop_front()
}

/// Take everything waiting.
///
/// Returns a `Vec` rather than holding the guard, so the lock is released before
/// any UI work happens — the producing thread must never wait on a repaint.
pub fn drain<T>(inbox: &Inbox<T>) -> Vec<T> {
    match inbox.lock() {
        Ok(mut queue) => queue.drain(..).collect(),
        Err(_) => Vec::new(),
    }
}

/// Non-reactive application state.
pub struct AppState {
    /// The layout's colours. A `LayoutTheme` and not a shared mutable struct:
    /// panel visibility and sizing live in signals, and the struct that used to
    /// shadow them read by nobody has been removed.
    pub layout_theme: LayoutTheme,
    pub file_browser: Rc<RefCell<FileBrowserState>>,
    pub buffer_manager: Rc<RefCell<BufferManager>>,
    pub terminal_tabs: Rc<RefCell<TerminalTabManager>>,
    pub lsp_handle: LspHandle,
}

/// Reactive signals driving the UI.
pub struct AppSignals {
    pub buffer_states: RwSignal<Vec<BufferState>>,
    pub active_buffer: RwSignal<Option<smithy_editor::buffer::BufferId>>,
    pub terminal_visible: RwSignal<bool>,
    pub agent_visible: RwSignal<bool>,
    pub sidebar_visible: RwSignal<bool>,
    pub editor_version: RwSignal<u64>,
    /// Bumped whenever the language server has said something. Carries no
    /// payload — read [`AppSignals::lsp_inbox`] for that.
    pub lsp_tick: RwSignal<Option<()>>,
    /// Everything the language server has said and the UI has not yet handled.
    ///
    /// Drained by exactly one effect, which dispatches on the message kind. Two
    /// effects both matching on a shared signal was the previous arrangement and
    /// is what lost messages.
    pub lsp_inbox: LspInbox,
    /// Which visual treatment the interface wears. Restored at startup.
    pub aesthetic: RwSignal<smithy_editor::Aesthetic>,
}

/// A write review, in all three of the places it lives.
///
/// One struct because a review has three parts and **they are only ever correct
/// together**: the queue the hook writes into, the diff currently on screen, and
/// the outcomes the model has not been told about yet. Keeping them as three
/// loose fields is what let a review outlive the project it was computed
/// against.
///
/// That was not cosmetic. A queued change stores a workspace-*relative* path,
/// and [`crate::agent::apply_change`] writes it through whichever root is live
/// at the moment it is accepted — which is correct, and is the rule the rest of
/// this file follows. So a review proposed in one project and accepted after a
/// switch resolves into the *next* project's tree, and the capability sandbox
/// passes it, because the path is perfectly legitimate there. That is the same
/// shape as the accident that overwrote this repository's own README.
///
/// [`abandon`](Self::abandon) is the answer, and it is a method rather than
/// three statements in `switch_project` so that the next person to add a fourth
/// piece of review state has somewhere obvious to put it.
#[derive(Clone)]
pub struct ReviewState {
    /// Queued by the write-review hook, on the tokio side.
    pub pending: Arc<Mutex<PendingChangeManager>>,
    /// The one being shown, if any.
    ///
    /// The whole `PendingFileChange`, not its diff: reviews are resolved by
    /// `tool_call_id`, and a modal holding only a diff has to guess which
    /// queued change it belongs to — by path, which is wrong exactly when one
    /// turn queues two writes to the same file.
    pub current: RwSignal<Option<smithy_editor::PendingFileChange>>,
    /// Review results the model has not been told about yet, delivered at the
    /// head of the next turn. See [`crate::agent::describe_review_outcome`] for
    /// why they cannot go back as the original tool result.
    ///
    /// `Rc<RefCell<_>>` rather than `Arc<Mutex<_>>`: only the UI thread ever
    /// touches this — the modal writes it, `submit_task` drains it — and both
    /// happen before the turn is handed to the runtime.
    pub outcomes: Rc<RefCell<Vec<String>>>,
}

impl ReviewState {
    pub fn new() -> Self {
        Self {
            pending: Arc::new(Mutex::new(PendingChangeManager::new())),
            current: RwSignal::new(None),
            outcomes: Rc::new(RefCell::new(Vec::new())),
        }
    }

    /// Drop every pending review, decided or not.
    ///
    /// Called on a project switch, where the alternative is writing one
    /// project's proposed change into another. The undelivered outcomes go with
    /// them: they describe files in a project the model is about to stop being
    /// grounded in, and the session that would have received them is torn down
    /// and rebuilt by the same switch.
    pub fn abandon(&self) {
        if let Ok(mut pending) = self.pending.lock() {
            pending.clear();
        }
        self.current.set(None);
        self.outcomes.borrow_mut().clear();
    }
}

impl Default for ReviewState {
    fn default() -> Self {
        Self::new()
    }
}

/// The agent side of the app: panel state, channels, and the live session.
#[derive(Clone)]
pub struct AgentState {
    pub panel: AgentPanelState,
    /// Events from the agent task to the UI.
    pub tx: Sender<AgentUiEvent>,
    /// Bumped when the agent task has said something. Payload is in
    /// [`AgentState::inbox`].
    pub tick: RwSignal<Option<()>>,
    /// Agent events waiting to be handled.
    pub inbox: Inbox<AgentUiEvent>,
    /// The live session, `None` until it connects.
    ///
    /// `tokio::sync::Mutex` rather than `std`: the guard is held across the
    /// `.await` of an entire turn, which a blocking mutex must never be.
    pub session: Arc<tokio::sync::Mutex<Option<Session>>>,
    /// Stops the running turn.
    ///
    /// Held separately from `session` on purpose: `submit_task` holds the
    /// session's async lock for the *entire* turn, so anything that has to
    /// interrupt that turn can never acquire it. A plain `std` mutex is right
    /// here — it is taken for a pointer read on the UI thread and never across
    /// an await. `None` until the session connects.
    pub stopper: Arc<Mutex<Option<smithy_agent::Stopper>>>,
    /// The project the agent is grounded in. Changing it rebuilds the session,
    /// because the project description lives in the frozen system prompt.
    pub project: Rc<RefCell<smithy_project::Project>>,
    /// Recent projects and per-project storage layout.
    pub registry: Rc<smithy_project::ProjectRegistry>,
    /// Sessions for the *current* project. Rebuilt when the project changes.
    pub sessions: Rc<RefCell<Option<smithy_agent::SessionStore>>>,
    /// The id of the session currently being appended to.
    pub session_id: Rc<RefCell<String>>,
    /// The file explorer, so switching project can re-root it.
    pub file_browser: Rc<RefCell<FileBrowserState>>,
    /// The Problems panel's contents.
    ///
    /// Held here so `switch_project` can empty it. Diagnostics are keyed by file
    /// and replaced per file, never merged — but nothing replaces a file that the
    /// new project simply does not have, so switching left the previous project's
    /// problems on screen and the next project's were *added* to them.
    pub diagnostics: DiagnosticsState,
    /// So switching project can re-root the language server too. Without this,
    /// the servers kept analysing whichever project was opened first — see
    /// `LspRegistry::current_client_for`.
    pub lsp_handle: LspHandle,
    /// So switching project can re-root the file watcher, which is the fourth
    /// thing that used to keep pointing at the tree you had left.
    ///
    /// `None` when the watcher could not start — a project on a filesystem that
    /// does not support notifications is still perfectly editable.
    pub file_watcher: Rc<RefCell<Option<smithy_editor::FileWatcherHandle>>>,
    /// So new terminals open in the project you are actually in. The cwd was set
    /// once at startup and never again, so after a switch every new shell opened
    /// in the previous project's root.
    pub terminal_tabs: Rc<RefCell<TerminalTabManager>>,
    /// Bumped to make the explorer rebuild its tree.
    pub file_browser_refresh: RwSignal<u64>,
    /// Everything a pending write review consists of. See [`ReviewState`].
    pub review: ReviewState,
    pub shell_approval_tx: Sender<ShellApprovalRequest>,
    /// The approval request currently on screen, if any.
    pub shell_approval: RwSignal<Option<ShellApprovalRequest>>,
    /// Bumped when another approval request arrives.
    pub shell_tick: RwSignal<Option<()>>,
    /// Requests waiting behind the one on screen.
    pub shell_inbox: Inbox<ShellApprovalRequest>,
}

/// Where Smithy should open, given a command-line path and the recents list.
///
/// The fallback order, and the reason for each step:
///
/// 1. **An explicit path.** `smithy ~/code/thing` is how you launch deliberately,
///    and an argument should always win.
/// 2. **The most recently opened project.** This is what every editor does, and the
///    registry already tracks it — `touch` runs on every project switch.
/// 3. **The launch directory**, which is only reachable on a genuinely first run.
///
/// Step 3 used to be the *only* rule, and it is why running `cargo run -p smithy`
/// inside this repository opened this repository — so the agent under test wrote
/// into the working tree it was being developed from. That is how a review accepted
/// in another project overwrote this one's README.
pub fn startup_project(
    arg: Option<PathBuf>,
    most_recent: Option<PathBuf>,
    launch_dir: PathBuf,
) -> PathBuf {
    // A path that no longer exists is worse than no path: it would strand the app
    // in a directory the user deleted or a drive they unplugged.
    if let Some(path) = arg.filter(|p| p.is_dir()) {
        return path;
    }
    if let Some(path) = most_recent.filter(|p| p.is_dir()) {
        return path;
    }
    launch_dir
}

/// The directory named on the command line, if one was.
fn path_argument() -> Option<PathBuf> {
    std::env::args_os().nth(1).map(PathBuf::from)
}

pub fn init_state() -> (AppState, AppSignals, AgentState) {
    let layout_theme = LayoutTheme::default();

    // The registry is read before anything else now, because the most recently
    // opened project is part of deciding where to start.
    let registry = smithy_project::ProjectRegistry::default_location()
        .or_else(|_| smithy_project::ProjectRegistry::new(std::env::temp_dir().join("smithy")))
        .expect("a project registry, even a temporary one");

    let start_dir = startup_project(
        path_argument(),
        registry.recents().first().map(|r| r.root.clone()),
        std::env::current_dir().unwrap_or_default(),
    );

    let file_browser_state = Rc::new(RefCell::new(FileBrowserState::new(start_dir.clone())));
    let buffer_manager: Rc<RefCell<BufferManager>> = Rc::new(RefCell::new(BufferManager::new()));
    let terminal_tabs = Rc::new(RefCell::new(TerminalTabManager::new()));

    // LSP manager on its own thread.
    let (lsp_manager, lsp_request_tx, lsp_response_rx) =
        LspManager::new(tokio_runtime().handle().clone());
    let lsp_handle = LspHandle::new(lsp_request_tx);
    std::thread::spawn(move || lsp_manager.run());
    lsp_handle.initialize(start_dir.clone());

    // Every bridge below goes through `bridge`, not through
    // `update_signal_from_channel` directly. See that function for why.
    let (lsp_tick, lsp_inbox) = bridge(lsp_response_rx);

    let signals = AppSignals {
        buffer_states: RwSignal::new(Vec::new()),
        active_buffer: RwSignal::new(None),
        terminal_visible: RwSignal::new(false),
        agent_visible: RwSignal::new(true),
        sidebar_visible: RwSignal::new(true),
        editor_version: RwSignal::new(0),
        lsp_tick,
        lsp_inbox,
        aesthetic: RwSignal::new(smithy_editor::Aesthetic::default()),
    };

    // Agent channels.
    // Streaming deltas arrive several per frame, so this was the lossiest bridge
    // in the app — and the least visible, because a dropped delta is hidden by the
    // final `Answered` carrying the whole text, and a dropped tool result just
    // leaves a step showing "Running" forever. Both read as flakiness.
    let (agent_tx, agent_rx) = unbounded::<AgentUiEvent>();
    let (agent_tick, agent_inbox) = bridge(agent_rx);

    // Rarer but worse: a turn can dispatch two `bash` calls at once, and a lost
    // request means its oneshot is dropped, so the hook denies a command the user
    // was never shown. The signal holds the one on screen; the inbox holds the
    // rest.
    let (shell_tx, shell_rx) = unbounded::<ShellApprovalRequest>();
    let shell_approval = RwSignal::new(None::<ShellApprovalRequest>);
    let (shell_tick, shell_inbox) = bridge(shell_rx);

    // Ground in whatever project encloses the launch directory, rather than in
    // the launch directory itself — starting inside `src/` should still put the
    // agent at the crate root.
    let start = file_browser_state.borrow().root_path.clone();
    let project = smithy_project::Project::discover(&start)
        .or_else(|_| smithy_project::Project::open(&start))
        .unwrap_or_else(|_| smithy_project::Project {
            root: start.clone(),
            name: "workspace".into(),
            kind: smithy_project::ProjectKind::Generic,
        });

    // New terminals open at the project root. Without this the PTY inherits the
    // editor's own working directory — wherever the binary was launched from —
    // so the terminal started somewhere unrelated to the code being edited.
    terminal_tabs.borrow_mut().set_cwd(project.root.clone());

    let _ = registry.touch(&project.root, &project.name);

    // Restore the saved look. Done here rather than at signal construction
    // because the data directory is only known once the registry exists.
    signals
        .aesthetic
        .set(smithy_editor::Aesthetic::load(registry.data_dir()));

    let sessions = smithy_agent::SessionStore::new(registry.sessions_dir(&project.root)).ok();

    let panel = AgentPanelState::new();
    // Attachments are labelled against this; without it every dropped file
    // inside the project would still be named by its absolute path.
    panel.project_root.set(project.root.clone());

    let agent = AgentState {
        panel,
        tx: agent_tx,
        tick: agent_tick,
        inbox: agent_inbox,
        session: Arc::new(tokio::sync::Mutex::new(None)),
        project: Rc::new(RefCell::new(project)),
        registry: Rc::new(registry),
        sessions: Rc::new(RefCell::new(sessions)),
        session_id: Rc::new(RefCell::new(new_session_id())),
        file_browser: file_browser_state.clone(),
        file_browser_refresh: RwSignal::new(0),
        review: ReviewState::new(),
        stopper: Arc::new(Mutex::new(None)),
        shell_approval_tx: shell_tx,
        shell_approval,
        shell_tick,
        shell_inbox,
        file_watcher: Rc::new(RefCell::new(None)),
        terminal_tabs: terminal_tabs.clone(),
        lsp_handle: lsp_handle.clone(),
        diagnostics: DiagnosticsState::new(),
    };

    let app_state = AppState {
        layout_theme,
        file_browser: file_browser_state,
        buffer_manager,
        terminal_tabs,
        lsp_handle: lsp_handle.clone(),
    };

    (app_state, signals, agent)
}

/// Connect to LM Studio in the background and build the session.
///
/// Deliberately not blocking startup: a missing or unloaded model should leave
/// you with a working editor and a red dot, not a window that refuses to open.
pub fn connect_agent(agent: &AgentState) {
    // Resume the project's most recent conversation, if there is one. Storing
    // sessions and then never offering them back would be the same gap as
    // before, just one layer deeper.
    let resume_from = agent
        .sessions
        .borrow()
        .as_ref()
        .and_then(|store| store.list().ok())
        .and_then(|mut sessions| {
            sessions.retain(|s| s.messages.len() > 1); // skip empty sessions
            sessions.into_iter().next()
        });
    spawn_session(agent, resume_from);
}

/// Throw away everything the model remembers and start again.
///
/// **Distinct from clearing the transcript, which is what the panel's other
/// clear does.** That one empties a `Vec` of view models; the conversation
/// itself — the `History` the session replays on every request, and the copy of
/// it on disk — was untouched, so the model still remembered a conversation you
/// could no longer see. This is the one that forgets.
///
/// It rebuilds rather than mutates, for the reason the whole crate is built
/// around: `History` is append-only so the endpoint's prefix cache stays warm,
/// and a `Session` that could truncate its own history would be a second way to
/// invalidate that cache. A fresh session is also the only way to get a freshly
/// *extracted* project context, since the system prompt is frozen at
/// construction — so a clear after a big refactor gives the model an accurate
/// map, which a truncation would not.
///
/// The old conversation is **not deleted**. Rolling `session_id` forward means
/// the next save writes a new file and the previous one stays on disk, so a
/// clear is recoverable and does not destroy work.
pub fn clear_context(agent: &AgentState) {
    // Stop first. A turn in flight holds the session lock for its whole
    // duration, so rebuilding under it would queue the new session behind the
    // old one finishing — and the answer would land in a transcript that had
    // already been cleared.
    if let Ok(stopper) = agent.stopper.lock() {
        if let Some(stopper) = stopper.as_ref() {
            stopper.stop();
        }
    }

    agent.panel.clear();
    agent.panel.busy.set(false);
    agent.panel.model_label.set("connecting…".into());
    agent.panel.connected.set(false);

    // Review outcomes are messages addressed to a model that is about to stop
    // existing: `prepend_review_outcomes` would otherwise open the first turn of
    // a brand-new conversation with "your proposed change to X was accepted",
    // about a change it never proposed. The pending *diffs* are deliberately
    // left alone — those are unreviewed work of yours, and dropping them to tidy
    // up a conversation would be destroying the wrong thing.
    agent.review.outcomes.borrow_mut().clear();

    *agent.session_id.borrow_mut() = new_session_id();
    spawn_session(agent, None);
}

/// Build a session in the background and hand it to the UI.
///
/// The single path both connecting and clearing take, so the two cannot drift:
/// the only difference between them is whether a stored conversation is replayed.
fn spawn_session(agent: &AgentState, resume_from: Option<smithy_agent::persist::StoredSession>) {
    let project = agent.project.borrow().clone();

    // Read on the UI thread on purpose: this is a small JSON file, and reading
    // it *here* is what makes reconnect pick up a setting the dialog just saved
    // without any signalling between the two.
    let config = smithy_agent::AgentConfig::load(agent.registry.data_dir());

    let tx = agent.tx.clone();
    let shell_tx = agent.shell_approval_tx.clone();
    let pending = agent.review.pending.clone();
    let slot = agent.session.clone();
    let stopper_slot = agent.stopper.clone();

    tokio_runtime().spawn(async move {
        match crate::agent::build_session(
            project,
            config,
            tx.clone(),
            shell_tx,
            pending,
            resume_from,
        )
        .await
        {
            Ok(handle) => {
                let model_label = handle.model_label.clone();
                let context_limit = handle.context_limit;
                let context_summary = handle.context_summary.clone();
                let restored = handle.restored.clone();
                let resumed_id = handle.session_id.clone();
                // Take the stop handle before the session disappears behind the
                // lock that a running turn holds for its whole duration.
                if let Ok(mut s) = stopper_slot.lock() {
                    *s = Some(handle.session.stopper());
                }
                *slot.lock().await = Some(handle.session);
                let _ = tx.send(AgentUiEvent::Ready {
                    model_label,
                    context_limit,
                    context_summary,
                    restored,
                    resumed_id,
                });
            }
            Err(e) => {
                let _ = tx.send(AgentUiEvent::Unavailable(e));
            }
        }
    });
}

/// Send a task to the agent.
pub fn submit_task(agent: &AgentState, task: String) {
    // The panel shows what the user typed. The model additionally receives any
    // review outcomes it has not been told about — those are IDE bookkeeping,
    // not something the user said, so they are not echoed into the transcript.
    // The panel already recorded each decision as a Notice when it was made.
    agent
        .panel
        .push(smithy_editor::AgentEntry::User(task.clone()));

    // Attached files go to the model but not into the bubble: a transcript in
    // which every message is preceded by three hundred lines of source is a
    // transcript you cannot read. What *is* recorded is that they were sent, as
    // a Notice — the same way a review decision is recorded — because a turn
    // whose answer depended on a file nobody can see afterwards is a turn you
    // cannot make sense of later.
    let attachments = agent.panel.attachments.get_untracked();
    let included: Vec<String> = attachments
        .iter()
        .filter(|a| a.included)
        .map(|a| a.display.clone())
        .collect();
    if !included.is_empty() {
        agent.panel.push(smithy_editor::AgentEntry::Notice(format!(
            "Attached {}: {}",
            match included.len() {
                1 => "1 file".to_string(),
                n => format!("{n} files"),
            },
            included.join(", ")
        )));
    }

    agent.panel.busy.set(true);
    agent.panel.streaming_answer.set(String::new());
    agent.panel.streaming_reasoning.set(String::new());

    // One message, one set of attachments. Carrying them forward would re-send
    // every file on every turn, which is both expensive and wrong — the model
    // already has them in history.
    agent.panel.clear_attachments();

    let task = crate::agent::prepend_review_outcomes(&mut agent.review.outcomes.borrow_mut(), task);

    let slot = agent.session.clone();
    let tx = agent.tx.clone();

    tokio_runtime().spawn(async move {
        // Attachments are read here rather than on the UI thread, and read at
        // send rather than at drop. Two separate reasons:
        //
        // - *Here*, because up to a megabyte of file can be cold on disk, and a
        //   blocking read on floem's main thread is a visible stall between
        //   pressing Send and the panel reacting.
        // - *At send*, so what the model sees is the file as it is now — drop a
        //   file, edit it, then send, and the edit counts.
        // The fallback is the message *without* its attachments, not an error
        // string: if the read worker dies, sending what the user typed is a
        // degraded turn, whereas replacing their words with a diagnostic throws
        // the request away and asks the model to answer the diagnostic.
        let unattached = task.clone();
        let task = tokio::task::spawn_blocking(move || {
            smithy_editor::attachment::materialize(&attachments, &task, |path| {
                std::fs::read_to_string(path).map_err(|e| e.to_string())
            })
        })
        .await
        .unwrap_or(unattached);

        let mut guard = slot.lock().await;
        match guard.as_mut() {
            Some(session) => crate::agent::run_turn(session, task, tx).await,
            None => {
                let _ = tx.send(AgentUiEvent::Unavailable(
                    "not connected to LM Studio yet".into(),
                ));
            }
        }
    });
}

/// Translate agent events into panel state. Runs on the UI thread.
/// Show the next shell approval request whenever the modal is free.
///
/// Separate from the modal itself because a request can arrive while one is
/// already on screen: the modal advances when answered, and this covers the case
/// where the queue was empty at that moment and filled afterwards.
pub fn setup_shell_approval_effect(agent: AgentState) {
    let tick = agent.shell_tick;
    let inbox = agent.shell_inbox.clone();
    let slot = agent.shell_approval;

    floem::reactive::Effect::new(move |_| {
        tick.get();
        if slot.get_untracked().is_none() {
            if let Some(next) = pop(&inbox) {
                slot.set(Some(next));
            }
        }
    });
}

pub fn setup_agent_effect(agent: AgentState) {
    let panel = agent.panel;
    let tick = agent.tick;
    let inbox = agent.inbox.clone();
    let current_diff = agent.review.current;
    let for_save = agent.clone();

    floem::reactive::Effect::new(move |_| {
        tick.get();
        // Every event, not just the last of the batch. Streaming deltas arrive
        // several per frame and the previous arrangement kept only one of them.
        for event in drain(&inbox) {
            match event {
                AgentUiEvent::Ready {
                    model_label,
                    context_limit,
                    context_summary,
                    restored,
                    resumed_id,
                } => {
                    if let Some(id) = resumed_id {
                        *for_save.session_id.borrow_mut() = id;
                    }
                    if !restored.is_empty() {
                        panel.clear();
                        for entry in restored {
                            panel.push(to_entry(entry));
                        }
                        panel.push(smithy_editor::AgentEntry::Notice(
                            "Resumed this project's last conversation.".into(),
                        ));
                    }
                    panel.model_label.set(model_label);
                    panel.context_limit.set(context_limit);
                    panel.context_label.set(context_summary);
                    panel.connected.set(true);
                }
                AgentUiEvent::Unavailable(reason) => {
                    panel.connected.set(false);
                    panel.model_label.set("disconnected".into());
                    panel.busy.set(false);
                    panel.push(smithy_editor::AgentEntry::Error(reason));
                }
                AgentUiEvent::Turn(turn) => apply_turn_event(&panel, turn),
                AgentUiEvent::ReviewRequested(diff) => {
                    // Raise the modal only if nothing is already under review; the
                    // rest queue behind it and surface as each is resolved.
                    if current_diff.get_untracked().is_none() {
                        current_diff.set(Some(diff));
                    }
                }
                AgentUiEvent::Answered(answer) => {
                    panel.streaming_answer.set(String::new());
                    panel.streaming_reasoning.set(String::new());
                    panel.push(smithy_editor::AgentEntry::Answer(answer));
                    panel.busy.set(false);
                    save_session(&for_save);
                }
                AgentUiEvent::Stopped(reason) => {
                    panel.streaming_answer.set(String::new());
                    panel.streaming_reasoning.set(String::new());
                    panel.push(smithy_editor::AgentEntry::Stopped(reason));
                    panel.busy.set(false);
                    // Saved even when the turn ended early: a stopped turn is still
                    // part of the conversation, and losing it would hide why.
                    save_session(&for_save);
                }
                AgentUiEvent::Failed(error) => {
                    panel.streaming_answer.set(String::new());
                    panel.streaming_reasoning.set(String::new());
                    panel.push(smithy_editor::AgentEntry::Error(error));
                    panel.busy.set(false);
                }
            }
        }
    });
}

/// Persist the current conversation to this project's session store.
///
/// Fire-and-forget: a failure to save is reported to stderr and never
/// interrupts the session. Losing a transcript is bad; losing the working
/// session because the disk was full would be worse.
pub fn save_session(agent: &AgentState) {
    let Some(store) = agent
        .sessions
        .borrow()
        .as_ref()
        .map(|s| s.root().to_path_buf())
    else {
        return;
    };
    let id = agent.session_id.borrow().clone();
    let project_root = agent.project.borrow().root.clone();
    // The model that produced this conversation. Passed as `""` before, so every
    // stored session claimed to have been produced by nothing.
    let model = agent.panel.model_label.get_untracked();
    let slot = agent.session.clone();

    tokio_runtime().spawn(async move {
        let guard = slot.lock().await;
        let Some(session) = guard.as_ref() else {
            return;
        };

        let stored = smithy_agent::persist::StoredSession::from_history(
            id,
            &project_root,
            &model,
            session.history(),
            session.sampling(),
            session.limits(),
        );
        drop(guard);

        match smithy_agent::SessionStore::new(store) {
            Ok(store) => {
                if let Err(e) = store.save(&stored) {
                    eprintln!("[session] could not save: {e}");
                }
            }
            Err(e) => eprintln!("[session] store unavailable: {e}"),
        }
    });
}

/// A fresh session id: unix seconds, the process, and a counter.
///
/// Sorts chronologically, and cannot collide — within a run because of the
/// counter, and *between* runs because of the process id. Without the latter,
/// two launches in the same second both minted `<secs>-000` and the second
/// overwrote the first's session file, which is a conversation lost to nothing
/// more exotic than starting the app twice quickly.
pub fn new_session_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    session_id(
        secs,
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed),
    )
}

/// The id format, as a pure function of its three inputs, so the uniqueness
/// claim above can be checked rather than asserted.
fn session_id(unix_seconds: u64, pid: u32, counter: u64) -> String {
    format!("{unix_seconds}-{pid:x}-{counter:03}")
}

/// Convert a restored transcript entry into a panel entry.
fn to_entry(entry: smithy_agent::TranscriptEntry) -> smithy_editor::AgentEntry {
    use smithy_agent::TranscriptEntry;
    use smithy_editor::{AgentEntry, StepStatus};
    match entry {
        TranscriptEntry::User(text) => AgentEntry::User(text),
        TranscriptEntry::Answer(text) => AgentEntry::Answer(text),
        TranscriptEntry::Step {
            id,
            name,
            arguments,
            content,
        } => AgentEntry::Step {
            id,
            // Step numbers belong to a turn that has ended; a restored step has
            // no meaningful position in the current one.
            step: 0,
            summary: smithy_editor::agent_panel::summarize_arguments(&arguments),
            name,
            status: StepStatus::Ok,
            detail: content,
        },
    }
}

fn apply_turn_event(panel: &AgentPanelState, event: TurnEvent) {
    use smithy_editor::{AgentEntry, StepStatus};

    match event {
        TurnEvent::Reasoning(chunk) => {
            panel.streaming_reasoning.update(|s| s.push_str(&chunk));
        }
        TurnEvent::Content(chunk) => {
            panel.streaming_answer.update(|s| s.push_str(&chunk));
        }
        TurnEvent::ToolStarted {
            id,
            step,
            name,
            arguments,
        } => {
            // The reasoning that produced this step has served its purpose once
            // the step itself is on screen.
            panel.streaming_reasoning.set(String::new());
            panel.push(AgentEntry::Step {
                id,
                step,
                summary: smithy_editor::agent_panel::summarize_arguments(&arguments),
                name,
                status: StepStatus::Running,
                detail: String::new(),
            });
        }
        TurnEvent::ToolFinished {
            id,
            content,
            is_error,
            ..
        } => {
            panel.resolve_step(&id, content, is_error);
        }
        TurnEvent::Warning(text) => {
            panel.push(AgentEntry::Notice(text));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An explicit path wins. `smithy ~/code/thing` is a deliberate instruction.
    #[test]
    fn a_path_argument_beats_everything() {
        let dir = tempfile::tempdir().expect("tempdir");
        let arg = dir.path().join("wanted");
        let recent = dir.path().join("recent");
        std::fs::create_dir_all(&arg).unwrap();
        std::fs::create_dir_all(&recent).unwrap();

        assert_eq!(
            startup_project(Some(arg.clone()), Some(recent), dir.path().to_path_buf()),
            arg
        );
    }

    /// With no argument, reopen what you were last working on — which is what
    /// every editor does, and what stops Smithy opening its own repository just
    /// because that is where `cargo run` was invoked.
    #[test]
    fn the_most_recent_project_beats_the_launch_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        let recent = dir.path().join("recent");
        std::fs::create_dir_all(&recent).unwrap();

        assert_eq!(
            startup_project(None, Some(recent.clone()), dir.path().to_path_buf()),
            recent
        );
    }

    /// First run: nothing remembered, nothing asked for. The launch directory is a
    /// reasonable guess *here* and only here.
    #[test]
    fn the_launch_directory_is_the_last_resort() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(
            startup_project(None, None, dir.path().to_path_buf()),
            dir.path()
        );
    }

    /// A remembered project can be deleted, renamed, or on an unplugged drive.
    /// Falling through is better than stranding the app somewhere that is gone.
    #[test]
    fn a_vanished_project_falls_through_instead_of_stranding() {
        let dir = tempfile::tempdir().expect("tempdir");
        let gone = dir.path().join("deleted-since");

        assert_eq!(
            startup_project(None, Some(gone.clone()), dir.path().to_path_buf()),
            dir.path(),
            "a recent that no longer exists must not be opened"
        );
        assert_eq!(
            startup_project(Some(gone), None, dir.path().to_path_buf()),
            dir.path(),
            "and neither must a mistyped argument"
        );
    }

    /// A file is not a project directory — `smithy foo.rs` should not root the
    /// workspace at a file.
    #[test]
    fn a_file_argument_is_not_treated_as_a_project() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("main.rs");
        std::fs::write(&file, "fn main() {}").unwrap();

        assert_eq!(
            startup_project(Some(file), None, dir.path().to_path_buf()),
            dir.path()
        );
    }

    /// Draining takes everything, in arrival order. Anything less is the defect
    /// this whole mechanism exists to avoid.
    #[test]
    fn draining_takes_every_message_in_order() {
        let inbox: Inbox<u32> = Arc::new(Mutex::new(std::collections::VecDeque::new()));
        inbox.lock().unwrap().extend([1, 2, 3]);

        assert_eq!(drain(&inbox), vec![1, 2, 3]);
        assert!(drain(&inbox).is_empty(), "draining twice must not repeat");
    }

    /// `pop` is FIFO. A stack would answer shell approvals in reverse, so the
    /// second command the model asked to run would be the first one presented —
    /// and the user would approve it out of order without knowing.
    #[test]
    fn popping_returns_the_oldest_first() {
        let inbox: Inbox<&str> = Arc::new(Mutex::new(std::collections::VecDeque::new()));
        inbox.lock().unwrap().extend(["first", "second"]);

        assert_eq!(pop(&inbox), Some("first"));
        assert_eq!(pop(&inbox), Some("second"));
        assert_eq!(pop(&inbox), None);
    }

    /// A review that outlives its project is written into the next one.
    ///
    /// The queued change carries a workspace-*relative* path and is applied
    /// through whichever root is live when it is accepted, so `README.md`
    /// proposed in one repository lands in whatever repository is open by the
    /// time somebody clicks Apply — and the capability sandbox permits it,
    /// because that path is entirely legitimate there.
    ///
    /// All three parts are asserted rather than an aggregate, because all three
    /// have to go: a queue that is emptied while the modal still shows its diff
    /// leaves an Apply button wired to a change nothing can find, and outcomes
    /// left behind describe files in a project the model is no longer in.
    #[test]
    fn abandoning_a_review_clears_the_queue_the_modal_and_the_undelivered_outcomes() {
        let review = ReviewState::new();

        let change = smithy_editor::PendingFileChange::new(
            "call_1",
            "README.md",
            "old\n".to_string(),
            "new\n".to_string(),
        );
        review.current.set(Some(change.clone()));
        review.pending.lock().unwrap().add(change);
        review
            .outcomes
            .borrow_mut()
            .push("Your proposed change to `README.md` was accepted.".into());

        review.abandon();

        assert!(
            review.pending.lock().unwrap().is_empty(),
            "a change left queued is applied against the next project's root"
        );
        assert!(
            review.current.get_untracked().is_none(),
            "a modal left open offers Apply for a change that is no longer queued"
        );
        assert!(
            review.outcomes.borrow().is_empty(),
            "an outcome left undelivered tells the next session about the previous project's files"
        );
    }

    /// **Two launches in the same second must not share a session id.**
    ///
    /// The id was `<unix seconds>-<per-process counter>`, and the counter starts
    /// at zero in every process — so starting Smithy twice inside one second
    /// produced `…-000` both times and the second run's first save overwrote the
    /// first run's conversation. Nothing reported it; the file was simply
    /// replaced.
    #[test]
    fn two_processes_starting_in_the_same_second_get_different_ids() {
        assert_ne!(
            session_id(1_700_000_000, 4_242, 0),
            session_id(1_700_000_000, 9_001, 0),
            "same second, same counter, different process — these are different sessions"
        );
    }

    /// And within one process the counter still separates them.
    #[test]
    fn two_sessions_in_one_process_get_different_ids() {
        assert_ne!(
            session_id(1_700_000_000, 4_242, 0),
            session_id(1_700_000_000, 4_242, 1)
        );
        assert_ne!(new_session_id(), new_session_id());
    }

    /// Ids are listed newest-first by `updated_at`, but a human reading the
    /// directory should still see them in order.
    #[test]
    fn ids_sort_chronologically() {
        let mut ids = [
            session_id(1_700_000_002, 1, 0),
            session_id(1_700_000_000, 1, 0),
            session_id(1_700_000_001, 1, 0),
        ];
        ids.sort();
        assert_eq!(ids[0], session_id(1_700_000_000, 1, 0));
        assert_eq!(ids[2], session_id(1_700_000_002, 1, 0));
    }

    /// An empty inbox is the normal case on most ticks, not an error.
    #[test]
    fn an_empty_inbox_yields_nothing_rather_than_blocking() {
        let inbox: Inbox<u8> = Arc::new(Mutex::new(std::collections::VecDeque::new()));
        assert!(drain(&inbox).is_empty());
        assert_eq!(pop(&inbox), None);
    }
}
