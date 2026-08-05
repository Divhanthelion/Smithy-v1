//! Application state.
//!
//! forge kept three overlapping copies of the same state — `AppState`,
//! `BufferManager`, and a pile of loose signals in `main.rs` — and its own
//! README named that as the top architectural problem. This is one layer:
//! non-reactive state in [`AppState`], reactive signals in [`AppSignals`], and
//! the agent's own state inside [`AgentPanelState`].

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use crossbeam_channel::{unbounded, Receiver, Sender};
use floem::ext_event::update_signal_from_channel;
use floem::reactive::{RwSignal, SignalGet, SignalUpdate};

use smithy_agent::{Session, TurnEvent};
use smithy_editor::{
    AgentPanelState, BufferManager, BufferState, DiagnosticsState, FileBrowserState, LayoutTheme,
    LspHandle, LspManager, LspResponse, PendingChangeManager, TerminalTabManager,
};

use crate::runtime::tokio_runtime;

/// One connection attempt in this process.
///
/// Generations are deliberately not persisted. Their only job is to make work
/// from an older async task distinguishable from the work the current window is
/// waiting for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GenerationId(u64);

impl GenerationId {
    #[cfg(test)]
    pub(crate) fn test(value: u64) -> Self {
        Self(value)
    }
}

/// One turn in this process.
///
/// The id is global across generations so a delayed Stop or terminal event
/// cannot become plausible again after reconnecting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TurnId(u64);

impl TurnId {
    #[cfg(test)]
    pub(crate) fn test(value: u64) -> Self {
        Self(value)
    }
}

/// The identity a session build was requested under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildStamp {
    generation: GenerationId,
    project_root: PathBuf,
}

impl BuildStamp {
    pub(crate) fn new(generation: GenerationId, project_root: PathBuf) -> Self {
        Self {
            generation,
            project_root,
        }
    }
}

/// The identity carried by an event crossing back to the UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentEventStamp {
    build: BuildStamp,
    turn: Option<TurnId>,
}

impl AgentEventStamp {
    fn for_build(build: BuildStamp) -> Self {
        Self { build, turn: None }
    }

    fn for_turn(build: BuildStamp, turn: TurnId) -> Self {
        Self {
            build,
            turn: Some(turn),
        }
    }

    /// A review registration that cannot alias the same tool-call id in another
    /// generation or turn.
    ///
    /// The length prefix makes the final component unambiguous even when a
    /// provider supplies ids containing separators. `PendingChangeManager` is
    /// keyed by strings, so the lifecycle identity has to be encoded at this
    /// boundary rather than kept only in the event envelope.
    #[cfg(test)]
    pub(crate) fn review_registration(&self, call_id: &str) -> Option<String> {
        Some(self.review_key(call_id)?.registration())
    }

    pub(crate) fn review_key(&self, call_id: &str) -> Option<smithy_editor::ReviewKey> {
        let turn = self.turn?;
        Some(smithy_editor::ReviewKey::new(
            self.build.generation.0,
            turn.0,
            call_id,
        ))
    }
}

/// The persistence identity installed with one live session.
///
/// The revision counter stays with this target across reconnects. A newly built
/// target gets a new id before it can accept a turn, so reconnect cannot select
/// an older file merely because the fresh conversation has not been saved yet.
#[derive(Clone)]
pub(crate) struct PersistenceTarget {
    store: Option<PathBuf>,
    session_id: Arc<Mutex<String>>,
    project_root: PathBuf,
    configured_model: String,
    binding: smithy_agent::persist::SessionBinding,
    revision: Arc<AtomicU64>,
}

impl PersistenceTarget {
    pub(crate) fn new(
        store: Option<PathBuf>,
        session_id: String,
        project_root: PathBuf,
        configured_model: String,
        binding: smithy_agent::persist::SessionBinding,
        revision: u64,
    ) -> Self {
        let revision = revision_counter(store.as_ref(), &session_id, revision);
        Self {
            store,
            session_id: Arc::new(Mutex::new(session_id)),
            project_root,
            configured_model,
            binding,
            revision,
        }
    }

    pub(crate) fn session_id(&self) -> String {
        self.session_id
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    pub(crate) fn binding(&self) -> &smithy_agent::persist::SessionBinding {
        &self.binding
    }

    pub(crate) fn revision(&self) -> u64 {
        self.revision.load(Ordering::Acquire)
    }

    /// Allocate the save before a turn starts.
    ///
    /// A delayed terminal task therefore owns an immutable id/revision even
    /// after New Session or a project switch replaces the visible target.
    fn next_save(&self) -> Option<SaveTarget> {
        let store = self.store.clone()?;
        let session_id = self.session_id();
        let revision = self
            .revision
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                current.checked_add(1)
            })
            .expect("session save revision exhausted")
            + 1;
        Some(SaveTarget {
            store,
            session_id,
            lineage: self.session_id.clone(),
            project_root: self.project_root.clone(),
            configured_model: self.configured_model.clone(),
            binding: self.binding.clone(),
            revision,
        })
    }

    pub(crate) fn snapshot(&self, session: &Session) -> smithy_agent::persist::StoredSession {
        let session_id = self.session_id();
        snapshot_session(
            &session_id,
            &self.project_root,
            &self.configured_model,
            self.binding.clone(),
            self.revision(),
            session,
        )
    }

    /// Compare an in-memory reconnect candidate with disk under the store's
    /// cross-process lease before deciding which bytes to replay.
    pub(crate) fn reconcile_snapshot(
        &self,
        snapshot: smithy_agent::persist::StoredSession,
    ) -> Result<
        (
            smithy_agent::persist::StoredSession,
            Option<String>,
        ),
        String,
    > {
        let Some(root) = self.store.clone() else {
            return Ok((snapshot, None));
        };
        match smithy_agent::SessionStore::new(root)?.save(&snapshot)? {
            smithy_agent::persist::SaveOutcome::Saved
            | smithy_agent::persist::SaveOutcome::Unchanged => Ok((snapshot, None)),
            smithy_agent::persist::SaveOutcome::Superseded { current } => {
                self.revision.fetch_max(current.revision, Ordering::AcqRel);
                Ok((
                    current,
                    Some(
                        "Resumed a newer on-disk continuation instead of an older in-memory \
                         snapshot."
                            .into(),
                    ),
                ))
            }
            smithy_agent::persist::SaveOutcome::Forked {
                original_id,
                forked,
                reason,
            } => {
                let mut lineage = self
                    .session_id
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                if *lineage == original_id {
                    *lineage = forked.id.clone();
                }
                self.revision.fetch_max(forked.revision, Ordering::AcqRel);
                Ok((
                    forked.clone(),
                    Some(format!(
                        "Session save conflict: preserved the in-memory branch as {} ({reason}).",
                        forked.id
                    )),
                ))
            }
        }
    }
}

/// Share revision allocation between every live target for one session file.
///
/// A retired turn can still be saving when switching away and back rebuilds the
/// same compatible session from disk. Separate counters then both issued
/// revision N+1 for divergent histories, and whichever rename won discarded the
/// other turn. The process-wide counter makes the rebuilt target continue after
/// every revision already allocated in this process, even before it reaches disk.
fn revision_counter(
    store: Option<&PathBuf>,
    session_id: &str,
    persisted_revision: u64,
) -> Arc<AtomicU64> {
    let Some(store) = store else {
        return Arc::new(AtomicU64::new(persisted_revision));
    };
    static COUNTERS: OnceLock<Mutex<HashMap<PathBuf, Arc<AtomicU64>>>> = OnceLock::new();
    let counters = COUNTERS.get_or_init(|| Mutex::new(HashMap::new()));
    let path = store.join(format!("{session_id}.json"));
    let mut counters = counters.lock().unwrap_or_else(|error| error.into_inner());
    let counter = counters
        .entry(path)
        .or_insert_with(|| Arc::new(AtomicU64::new(persisted_revision)))
        .clone();
    counter.fetch_max(persisted_revision, Ordering::AcqRel);
    counter
}

/// An immutable destination for one terminal save.
#[derive(Clone)]
struct SaveTarget {
    store: PathBuf,
    session_id: String,
    lineage: Arc<Mutex<String>>,
    project_root: PathBuf,
    configured_model: String,
    binding: smithy_agent::persist::SessionBinding,
    revision: u64,
}

impl SaveTarget {
    fn snapshot(&self, session: &Session) -> smithy_agent::persist::StoredSession {
        snapshot_session(
            &self.session_id,
            &self.project_root,
            &self.configured_model,
            self.binding.clone(),
            self.revision,
            session,
        )
    }

    fn persist(
        &self,
        session: &smithy_agent::persist::StoredSession,
    ) -> Result<smithy_agent::persist::SaveOutcome, String> {
        let outcome = smithy_agent::SessionStore::new(self.store.clone())?.save(session)?;
        if let smithy_agent::persist::SaveOutcome::Forked {
            original_id,
            forked,
            ..
        } = &outcome
        {
            let mut lineage = self
                .lineage
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if *lineage == *original_id {
                *lineage = forked.id.clone();
            }
        }
        Ok(outcome)
    }

    fn independent_lineage(&self, session_id: String) -> Self {
        Self {
            store: self.store.clone(),
            session_id: session_id.clone(),
            lineage: Arc::new(Mutex::new(session_id)),
            project_root: self.project_root.clone(),
            configured_model: self.configured_model.clone(),
            binding: self.binding.clone(),
            revision: self.revision,
        }
    }

    fn retarget(&self, session_id: String) -> Self {
        Self {
            session_id,
            ..self.clone()
        }
    }
}

fn snapshot_session(
    id: &str,
    project_root: &std::path::Path,
    configured_model: &str,
    binding: smithy_agent::persist::SessionBinding,
    revision: u64,
    session: &Session,
) -> smithy_agent::persist::StoredSession {
    smithy_agent::persist::StoredSession::from_session_state(
        id,
        project_root,
        configured_model,
        binding,
        revision,
        session.history(),
        session.sampling(),
        session.limits(),
        session.reasoning().to_vec(),
        session.turn_outcomes().to_vec(),
        session.accounting(),
    )
}

fn completed_turn_snapshot(
    save: Option<SaveTarget>,
    session: &Session,
) -> Option<(SaveTarget, smithy_agent::persist::StoredSession)> {
    save.map(|target| {
        let stored = target.snapshot(session);
        (target, stored)
    })
}

async fn persist_completed_turn(
    pending: Option<(SaveTarget, smithy_agent::persist::StoredSession)>,
    queue: PendingSaveQueue,
    events: AgentEventSender,
) {
    let Some(pending) = pending else {
        return;
    };
    match queue.enqueue(pending) {
        QueueAction::Start {
            key,
            wake,
            hard_notice,
        } => {
            if let Some(message) = hard_notice {
                events.send_persistence(PersistenceStatus::HardFailure(message));
            }
            let (first_attempt, observed) = tokio::sync::oneshot::channel();
            tokio::spawn(run_lineage_worker(
                queue,
                key,
                wake,
                events,
                Some(first_attempt),
            ));
            let _ = observed.await;
        }
        QueueAction::Wake(wake) => wake.notify_one(),
        QueueAction::Ignored => {}
        QueueAction::Hard(message) => {
            events.send_persistence(PersistenceStatus::HardFailure(message));
        }
    }
}

async fn run_lineage_worker(
    queue: PendingSaveQueue,
    mut key: PendingLineageKey,
    wake: Arc<tokio::sync::Notify>,
    events: AgentEventSender,
    mut first_attempt: Option<tokio::sync::oneshot::Sender<()>>,
) {
    loop {
        let Some((generation, pending)) = queue.snapshot(&key) else {
            signal_first_attempt(&mut first_attempt);
            return;
        };
        let result = tokio::task::spawn_blocking(move || {
            pending.target.persist(&pending.stored)
        })
        .await;
        match result {
            Ok(Ok(outcome)) => {
                let next = queue.complete_success(&key, generation, &outcome);
                surface_save_outcome(&events, &outcome, queue.has_pending());
                match next {
                    WorkerNext::Done => {
                        signal_first_attempt(&mut first_attempt);
                        return;
                    }
                    WorkerNext::Continue(next) => {
                        key = next;
                        signal_first_attempt(&mut first_attempt);
                    }
                }
            }
            Ok(Err(error)) => {
                match queue.record_failure(&key, generation) {
                    FailureNext::RetryNow => {
                        signal_first_attempt(&mut first_attempt);
                        continue;
                    }
                    FailureNext::Wait { delay, announce } => {
                        if announce {
                            events.send_persistence(PersistenceStatus::Failed(format!(
                                "Session save pending; Smithy is retrying the latest snapshot: \
                                 {error}"
                            )));
                        }
                        signal_first_attempt(&mut first_attempt);
                        tokio::select! {
                            _ = tokio::time::sleep(delay) => {}
                            _ = wake.notified() => {}
                        }
                    }
                    FailureNext::Done => {
                        signal_first_attempt(&mut first_attempt);
                        return;
                    }
                }
            }
            Err(error) => {
                let message = format!("session save worker stopped: {error}");
                match queue.record_failure(&key, generation) {
                    FailureNext::RetryNow => continue,
                    FailureNext::Wait { delay, announce } => {
                        if announce {
                            events.send_persistence(PersistenceStatus::Failed(format!(
                                "Session save pending; Smithy is retrying the latest snapshot: \
                                 {message}"
                            )));
                        }
                        signal_first_attempt(&mut first_attempt);
                        tokio::select! {
                            _ = tokio::time::sleep(delay) => {}
                            _ = wake.notified() => {}
                        }
                    }
                    FailureNext::Done => return,
                }
            }
        }
    }
}

fn signal_first_attempt(first_attempt: &mut Option<tokio::sync::oneshot::Sender<()>>) {
    if let Some(sender) = first_attempt.take() {
        let _ = sender.send(());
    }
}

fn surface_save_outcome(
    events: &AgentEventSender,
    outcome: &smithy_agent::persist::SaveOutcome,
    pending_failures_remain: bool,
) {
    match outcome {
        smithy_agent::persist::SaveOutcome::Forked { forked, reason, .. } => {
            events.send_persistence(PersistenceStatus::Conflict(format!(
                "Session save conflict: preserved this branch as {} ({reason}).",
                forked.id
            )));
        }
        smithy_agent::persist::SaveOutcome::Superseded { .. } => {
            events.send_persistence(PersistenceStatus::Conflict(
                "A newer on-disk continuation superseded an older pending snapshot.".into(),
            ));
        }
        smithy_agent::persist::SaveOutcome::Saved
        | smithy_agent::persist::SaveOutcome::Unchanged => {
            if !pending_failures_remain {
                events.send_persistence(PersistenceStatus::Recovered);
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct PendingLineageKey {
    store: PathBuf,
    session_id: String,
}

#[derive(Clone)]
struct PendingSave {
    target: SaveTarget,
    stored: smithy_agent::persist::StoredSession,
}

struct PendingLineage {
    pending: PendingSave,
    generation: u64,
    failures: u32,
    wake: Arc<tokio::sync::Notify>,
}

#[derive(Default)]
struct PendingSaveState {
    lineages: HashMap<PendingLineageKey, PendingLineage>,
    /// One snapshot retained after the hard stop catches an already-completing
    /// turn. It is not retried automatically because no bounded lineage slot
    /// remains; the panel keeps the session visibly blocked.
    hard_unsaved: Option<PendingSave>,
}

#[derive(Clone)]
struct PendingSaveQueue {
    inner: Arc<Mutex<PendingSaveState>>,
    hard_stop: Arc<AtomicBool>,
    workers_started: Arc<AtomicU64>,
}

impl Default for PendingSaveQueue {
    fn default() -> Self {
        Self::new(Arc::new(AtomicBool::new(false)))
    }
}

impl PendingSaveQueue {
    fn new(hard_stop: Arc<AtomicBool>) -> Self {
        Self {
            inner: Arc::new(Mutex::new(PendingSaveState::default())),
            hard_stop,
            workers_started: Arc::new(AtomicU64::new(0)),
        }
    }

    fn enqueue(
        &self,
        (mut target, mut stored): (SaveTarget, smithy_agent::persist::StoredSession),
    ) -> QueueAction {
        let mut key = PendingLineageKey {
            store: target.store.clone(),
            session_id: target.session_id.clone(),
        };
        let mut state = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        if let Some(lineage) = state.lineages.get_mut(&key) {
            return match smithy_agent::persist::snapshot_relation(
                &lineage.pending.stored,
                &stored,
            ) {
                Ok(smithy_agent::persist::SnapshotRelation::ExistingPrefix) => {
                    lineage.pending = PendingSave { target, stored };
                    lineage.generation = lineage.generation.saturating_add(1);
                    lineage.failures = 0;
                    QueueAction::Wake(lineage.wake.clone())
                }
                Ok(smithy_agent::persist::SnapshotRelation::Equal)
                    if stored.revision >= lineage.pending.stored.revision =>
                {
                    lineage.pending = PendingSave { target, stored };
                    lineage.generation = lineage.generation.saturating_add(1);
                    lineage.failures = 0;
                    QueueAction::Wake(lineage.wake.clone())
                }
                Ok(smithy_agent::persist::SnapshotRelation::CandidatePrefix)
                | Ok(smithy_agent::persist::SnapshotRelation::Equal) => QueueAction::Ignored,
                Ok(smithy_agent::persist::SnapshotRelation::Diverged) | Err(_) => {
                    let forked_id = match smithy_agent::persist::conflict_session_id(
                        &target.session_id,
                    ) {
                        Ok(id) => id,
                        Err(error) => {
                            return self.hard_stop_locked(
                                &mut state,
                                PendingSave { target, stored },
                                format!("Cannot preserve divergent session branch: {error}"),
                            )
                        }
                    };
                    target = target.independent_lineage(forked_id.clone());
                    stored.id = forked_id.clone();
                    key = PendingLineageKey {
                        store: target.store.clone(),
                        session_id: forked_id,
                    };
                    self.insert_lineage_locked(&mut state, key, PendingSave { target, stored })
                }
            };
        }
        if self.hard_stop.load(Ordering::Acquire) {
            return self.hard_stop_locked(
                &mut state,
                PendingSave { target, stored },
                "Session persistence is already hard-stopped with unsaved branches.".into(),
            );
        }
        self.insert_lineage_locked(&mut state, key, PendingSave { target, stored })
    }

    fn insert_lineage_locked(
        &self,
        state: &mut PendingSaveState,
        key: PendingLineageKey,
        pending: PendingSave,
    ) -> QueueAction {
        if state.lineages.len() >= MAX_PENDING_LINEAGES {
            return self.hard_stop_locked(
                state,
                pending,
                format!(
                    "Session persistence reached its safety bound of {MAX_PENDING_LINEAGES} \
                     unsaved branches. Further turns are disabled until Smithy is restarted \
                     after the store is made writable."
                ),
            );
        }
        let wake = Arc::new(tokio::sync::Notify::new());
        state.lineages.insert(
            key.clone(),
            PendingLineage {
                pending,
                generation: 1,
                failures: 0,
                wake: wake.clone(),
            },
        );
        self.workers_started.fetch_add(1, Ordering::Relaxed);
        let hard_notice = if state.lineages.len() >= MAX_PENDING_LINEAGES {
            self.hard_stop.store(true, Ordering::Release);
            Some(format!(
                "Session persistence reached its safety bound of {MAX_PENDING_LINEAGES} unsaved \
                 branches. Further turns are disabled until pending saves recover."
            ))
        } else {
            None
        };
        QueueAction::Start {
            key,
            wake,
            hard_notice,
        }
    }

    fn hard_stop_locked(
        &self,
        state: &mut PendingSaveState,
        pending: PendingSave,
        mut message: String,
    ) -> QueueAction {
        self.hard_stop.store(true, Ordering::Release);
        match state.hard_unsaved.as_mut() {
            None => state.hard_unsaved = Some(pending),
            Some(retained) => match smithy_agent::persist::snapshot_relation(
                &retained.stored,
                &pending.stored,
            ) {
                Ok(smithy_agent::persist::SnapshotRelation::ExistingPrefix) => {
                    *retained = pending;
                }
                Ok(smithy_agent::persist::SnapshotRelation::Equal)
                    if pending.stored.revision >= retained.stored.revision =>
                {
                    *retained = pending;
                }
                Ok(smithy_agent::persist::SnapshotRelation::CandidatePrefix)
                | Ok(smithy_agent::persist::SnapshotRelation::Equal) => {}
                Ok(smithy_agent::persist::SnapshotRelation::Diverged) | Err(_) => {
                    message.push_str(
                        " An additional divergent completion arrived after the hard bound; it \
                         could not be retained in the bounded retry pool.",
                    );
                }
            },
        }
        QueueAction::Hard(message)
    }

    fn snapshot(&self, key: &PendingLineageKey) -> Option<(u64, PendingSave)> {
        self.inner
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .lineages
            .get(key)
            .map(|lineage| (lineage.generation, lineage.pending.clone()))
    }

    fn complete_success(
        &self,
        key: &PendingLineageKey,
        generation: u64,
        outcome: &smithy_agent::persist::SaveOutcome,
    ) -> WorkerNext {
        let mut state = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        let Some(lineage) = state.lineages.get(key) else {
            return WorkerNext::Done;
        };
        if lineage.generation == generation {
            state.lineages.remove(key);
            if state.hard_unsaved.is_none() && state.lineages.len() < MAX_PENDING_LINEAGES {
                self.hard_stop.store(false, Ordering::Release);
            }
            return WorkerNext::Done;
        }
        if let smithy_agent::persist::SaveOutcome::Forked { forked, .. } = outcome {
            let mut lineage = state.lineages.remove(key).expect("lineage checked above");
            lineage.pending.target = lineage.pending.target.retarget(forked.id.clone());
            lineage.pending.stored.id = forked.id.clone();
            let next = PendingLineageKey {
                store: key.store.clone(),
                session_id: forked.id.clone(),
            };
            state.lineages.insert(next.clone(), lineage);
            WorkerNext::Continue(next)
        } else {
            WorkerNext::Continue(key.clone())
        }
    }

    fn record_failure(&self, key: &PendingLineageKey, generation: u64) -> FailureNext {
        let mut state = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        let Some(lineage) = state.lineages.get_mut(key) else {
            return FailureNext::Done;
        };
        if lineage.generation != generation {
            return FailureNext::RetryNow;
        }
        lineage.failures = lineage.failures.saturating_add(1);
        FailureNext::Wait {
            delay: retry_delay(lineage.failures),
            announce: lineage.failures == 1,
        }
    }

    fn has_pending(&self) -> bool {
        let state = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        !state.lineages.is_empty() || state.hard_unsaved.is_some()
    }
}

enum QueueAction {
    Start {
        key: PendingLineageKey,
        wake: Arc<tokio::sync::Notify>,
        hard_notice: Option<String>,
    },
    Wake(Arc<tokio::sync::Notify>),
    Ignored,
    Hard(String),
}

enum WorkerNext {
    Done,
    Continue(PendingLineageKey),
}

enum FailureNext {
    RetryNow,
    Wait {
        delay: std::time::Duration,
        announce: bool,
    },
    Done,
}

/// A quarter second catches a transient rename/permission race quickly; the
/// thirty-second cap prevents a permanently unwritable volume from spinning or
/// going silent forever.
const INITIAL_SAVE_RETRY_MS: u64 = 250;
const MAX_SAVE_RETRY_SECS: u64 = 30;
/// Seven retrying branches plus one emergency completed snapshot cap retained
/// full histories at eight. Reaching seven hard-stops new turns so the emergency
/// slot can catch the turn already unwinding; append-only updates still coalesce.
const MAX_PENDING_LINEAGES: usize = 7;

fn retry_delay(failures: u32) -> std::time::Duration {
    let exponent = failures.saturating_sub(1).min(7);
    let millis = INITIAL_SAVE_RETRY_MS.saturating_mul(1_u64 << exponent);
    std::time::Duration::from_millis(
        millis.min(MAX_SAVE_RETRY_SECS.saturating_mul(1_000)),
    )
}

/// Everything the agent task sends back to the UI thread.
#[derive(Clone)]
pub struct AgentUiEvent {
    pub stamp: AgentEventStamp,
    pub kind: AgentUiEventKind,
}

/// The payload inside a generation-stamped agent event.
#[derive(Clone)]
pub enum AgentUiEventKind {
    /// The session connected; carries the model label and derived budget.
    Ready {
        model_label: String,
        context_limit: i64,
        context_summary: String,
        /// Transcript of a resumed conversation; empty for a fresh session.
        restored: Vec<smithy_agent::TranscriptEntry>,
        /// The installed target, whether resumed or fresh.
        session_id: String,
        /// Precise reason a saved/current conversation was not replayed.
        resume_notice: Option<String>,
        /// Restored cost/context baseline before another completion arrives.
        context_usage: Option<(i64, smithy_editor::ContextUsageSnapshot)>,
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
    /// Save state is process-wide and must surface even if the originating turn
    /// became stale during a reconnect or project switch.
    Persistence(PersistenceStatus),
    /// Stashed once per completion — never computed on the paint path.
    ContextUsage {
        prompt_tokens: i64,
        snapshot: smithy_editor::ContextUsageSnapshot,
    },
}

#[derive(Clone)]
pub enum PersistenceStatus {
    Failed(String),
    Conflict(String),
    HardFailure(String),
    Recovered,
}

/// A generation-bound route into the shared UI channel.
///
/// The write and shell hooks are frozen into a session's tool registry, so they
/// cannot be handed a turn id as an ordinary call argument. This route owns the
/// one mutable word they need. A transition clears it before abandoning the
/// gates, so a hook that starts late cannot raise an old modal.
#[derive(Clone)]
pub struct AgentEventSender {
    tx: Sender<AgentUiEvent>,
    build: BuildStamp,
    active_turn: Arc<AtomicU64>,
}

/// Zero is reserved for "no active turn"; without a sentinel a hook created
/// between turns could stamp its request as a real first turn.
const NO_ACTIVE_TURN: u64 = 0;

impl AgentEventSender {
    pub(crate) fn new(tx: Sender<AgentUiEvent>, build: BuildStamp) -> Self {
        Self {
            tx,
            build,
            active_turn: Arc::new(AtomicU64::new(NO_ACTIVE_TURN)),
        }
    }

    fn send_build(&self, kind: AgentUiEventKind) -> bool {
        self.tx
            .send(AgentUiEvent {
                stamp: AgentEventStamp::for_build(self.build.clone()),
                kind,
            })
            .is_ok()
    }

    fn send_persistence(&self, status: PersistenceStatus) -> bool {
        self.send_build(AgentUiEventKind::Persistence(status))
    }

    pub(crate) fn active_stamp(&self) -> Option<AgentEventStamp> {
        let turn = self.active_turn.load(Ordering::Acquire);
        (turn != NO_ACTIVE_TURN)
            .then(|| AgentEventStamp::for_turn(self.build.clone(), TurnId(turn)))
    }

    pub(crate) fn is_active(&self, stamp: &AgentEventStamp) -> bool {
        stamp.build == self.build
            && stamp.turn.is_some_and(|turn| {
                self.active_turn.load(Ordering::Acquire) == turn.0
            })
    }

    pub(crate) fn send_turn(&self, stamp: &AgentEventStamp, kind: AgentUiEventKind) -> bool {
        if !self.is_active(stamp) {
            return false;
        }
        self.tx
            .send(AgentUiEvent {
                stamp: stamp.clone(),
                kind,
            })
            .is_ok()
    }

    /// Send the terminal event after the lifecycle has released Stop ownership.
    ///
    /// This deliberately does not require an active turn: releasing that slot
    /// first is what prevents a terminal event from painting Send as available
    /// while Stop can still target the completed turn. The UI's generation and
    /// current-turn check remains the authority if a transition lands between
    /// release and send.
    pub(crate) fn send_finished(
        &self,
        stamp: &AgentEventStamp,
        kind: AgentUiEventKind,
    ) -> bool {
        if stamp.build != self.build || stamp.turn.is_none() {
            return false;
        }
        self.tx
            .send(AgentUiEvent {
                stamp: stamp.clone(),
                kind,
            })
            .is_ok()
    }

    pub(crate) fn activate(&self, turn: TurnId) {
        self.active_turn.store(turn.0, Ordering::Release);
    }

    fn deactivate(&self, turn: TurnId) {
        let _ = self.active_turn.compare_exchange(
            turn.0,
            NO_ACTIVE_TURN,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }

    pub(crate) fn retire(&self) {
        self.active_turn
            .store(NO_ACTIVE_TURN, Ordering::Release);
    }
}

/// Why a shell approval waiter woke up.
pub(crate) enum ShellApprovalDecision {
    Approved,
    Denied,
    Abandoned,
}

/// A shell command waiting for the user's go-ahead.
///
/// The responder is `Arc<Mutex<Option<..>>>` because the request travels through
/// a reactive signal, which requires `Clone`, while a oneshot sender is
/// single-use.
#[derive(Clone)]
pub struct ShellApprovalRequest {
    pub command: String,
    pub stamp: AgentEventStamp,
    pub(crate) responder:
        Arc<Mutex<Option<tokio::sync::oneshot::Sender<ShellApprovalDecision>>>>,
}

impl ShellApprovalRequest {
    pub(crate) fn is_pending(&self) -> bool {
        self.responder
            .lock()
            .map(|slot| slot.is_some())
            .unwrap_or(false)
    }

    /// Answer the request. Safe to call more than once; only the first wins.
    pub fn respond(&self, approve: bool) {
        self.answer(if approve {
            ShellApprovalDecision::Approved
        } else {
            ShellApprovalDecision::Denied
        });
    }

    /// Withdraw a prompt whose turn no longer exists.
    pub fn abandon(&self) {
        self.answer(ShellApprovalDecision::Abandoned);
    }

    fn answer(&self, decision: ShellApprovalDecision) {
        if let Ok(mut slot) = self.responder.lock() {
            if let Some(tx) = slot.take() {
                let _ = tx.send(decision);
            }
        }
    }
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
/// That was not cosmetic. Reviews now retain the canonical root identity and
/// exact file base used for preview, and acceptance refuses a different
/// generation, turn or directory object. Abandoning remains necessary so stale
/// UI cannot offer an Apply button that is guaranteed to fail.
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
    /// The whole `PendingFileChange`, not its diff: reviews are resolved by its
    /// lifecycle-qualified registration, and a modal holding only a diff has to
    /// guess which queued change it belongs to — by path, which is wrong exactly
    /// when one turn queues two writes to the same file.
    pub current: RwSignal<Option<smithy_editor::PendingFileChange>>,
    /// Review results the model has not been told about yet, delivered at the
    /// head of the next turn.
    ///
    /// **Now a fallback rather than the main path.** With a blocking gate the
    /// outcome goes back as the tool's own result, which is where the model
    /// looks for it. This catches a decision whose tool call had already gone
    /// away; a session transition clears it because the next model does not own
    /// that proposal.
    ///
    /// `Rc<RefCell<_>>` rather than `Arc<Mutex<_>>`: only the UI thread ever
    /// touches this — the modal writes it, `submit_task` drains it — and both
    /// happen before the turn is handed to the runtime.
    pub outcomes: Rc<RefCell<Vec<String>>>,
    /// Where a blocked `edit`/`write` call is waiting, keyed by the
    /// generation/turn-qualified registration in `PendingFileChange::id`.
    ///
    /// **This is what makes the gate observable.** Before it, the hook denied
    /// the tool and the outcome arrived only at the start of the *next* turn —
    /// so inside one long turn the model never learned whether any of its edits
    /// had landed. A measured session spent 26 of 76 tool calls re-editing and
    /// polling files whose edits were sitting approved on disk. The tool now
    /// suspends here until the modal answers, exactly as shell approval already
    /// did.
    pub responders: Arc<Mutex<HashMap<String, tokio::sync::oneshot::Sender<ReviewOutcome>>>>,
    /// When true the gate is off: writes go straight to disk with no modal.
    ///
    /// An `AtomicBool` and not a signal because the hook reads it from the tokio
    /// side, where floem's reactive graph cannot be touched.
    pub auto_approve: Arc<AtomicBool>,
}

/// What the modal decided, as the tool will hear it.
#[derive(Debug, Clone)]
pub struct ReviewOutcome {
    /// The sentence handed back to the model.
    pub message: String,
    /// Whether anything was written. Drives success versus error, which is the
    /// difference between the model continuing and the model investigating.
    pub applied: bool,
}

impl ReviewState {
    pub fn new() -> Self {
        Self {
            pending: Arc::new(Mutex::new(PendingChangeManager::new())),
            current: RwSignal::new(None),
            outcomes: Rc::new(RefCell::new(Vec::new())),
            responders: Arc::new(Mutex::new(HashMap::new())),
            auto_approve: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Answer a blocked tool call, if one is waiting on this id.
    ///
    /// Returns whether anybody was listening. `false` means the outcome has to
    /// go through [`ReviewState::outcomes`] instead — the turn it belonged to is
    /// gone.
    pub fn respond(&self, id: &str, outcome: ReviewOutcome) -> bool {
        let Ok(mut responders) = self.responders.lock() else {
            return false;
        };
        match responders.remove(id) {
            Some(tx) => tx.send(outcome).is_ok(),
            None => false,
        }
    }

    /// Drop every pending review, decided or not.
    ///
    /// Called on every session transition. On a project switch the alternative
    /// is writing one project's proposed change into another; on New Session or
    /// reconnect it is delivering a decision to a model history that no longer
    /// owns the tool call. The undelivered outcomes go with the retired session.
    pub fn abandon(&self) {
        // Answer anyone still blocked before dropping the queue. A tool waiting
        // on a oneshot whose sender is dropped sees `Err` and reports the modal
        // as dismissed, which is true but says nothing about why — and a turn
        // stalled behind a project switch is worth naming.
        if let Ok(mut responders) = self.responders.lock() {
            for (_, tx) in responders.drain() {
                let _ = tx.send(ReviewOutcome {
                    message: "the review was abandoned because the agent session changed. The \
                              file was not written."
                        .to_string(),
                    applied: false,
                });
            }
        }
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

#[derive(Debug, Clone, PartialEq, Eq)]
struct TurnStamp {
    build: BuildStamp,
    turn: TurnId,
}

impl TurnStamp {
    fn event(&self) -> AgentEventStamp {
        AgentEventStamp::for_turn(self.build.clone(), self.turn)
    }
}

/// A barrier between a claimed turn and reconnect's in-memory snapshot.
///
/// A turn claims its lifecycle slot before attachment I/O, while the session
/// mutex is acquired afterwards on the runtime. Reconnect used to race into
/// that gap, snapshot the pre-turn History, and then install it as the successor
/// of the revision already reserved for the missing turn. The barrier is marked
/// synchronously with the claim and released only after the completed snapshot
/// has been taken.
#[derive(Clone)]
pub(crate) struct SessionQuiescence {
    inner: Arc<SessionQuiescenceInner>,
}

struct SessionQuiescenceInner {
    active: AtomicBool,
    idle: tokio::sync::Notify,
}

impl SessionQuiescence {
    fn begin_turn(&self) -> bool {
        self.inner
            .active
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    fn finish_turn(&self) {
        if self.inner.active.swap(false, Ordering::AcqRel) {
            self.inner.idle.notify_waiters();
        }
    }

    pub(crate) async fn wait_until_idle(&self) {
        loop {
            // Register before checking the flag. If finish lands between these
            // two lines, either the flag is already false or this future owns
            // the notification; there is no missed-wakeup window.
            let idle = self.inner.idle.notified();
            if !self.inner.active.load(Ordering::Acquire) {
                return;
            }
            idle.await;
        }
    }
}

impl Default for SessionQuiescence {
    fn default() -> Self {
        Self {
            inner: Arc::new(SessionQuiescenceInner {
                active: AtomicBool::new(false),
                idle: tokio::sync::Notify::new(),
            }),
        }
    }
}

/// The pure identity half of the lifecycle.
///
/// Keeping these decisions free of sessions, providers and floem signals makes
/// the dangerous orderings directly testable: A/B/C builds can finish in any
/// order, and an old turn can finish after a new one starts.
#[derive(Debug)]
struct LifecycleIdentity {
    generation: GenerationId,
    project_root: PathBuf,
    next_turn: u64,
    installed: Option<BuildStamp>,
    current_turn: Option<TurnStamp>,
    active_turn: Option<TurnStamp>,
}

impl LifecycleIdentity {
    fn new(project_root: PathBuf) -> Self {
        Self {
            generation: GenerationId(0),
            project_root,
            next_turn: 0,
            installed: None,
            current_turn: None,
            active_turn: None,
        }
    }

    fn transition(&mut self, project_root: PathBuf) -> BuildStamp {
        self.generation = GenerationId(
            self.generation
                .0
                .checked_add(1)
                .expect("agent generation exhausted"),
        );
        self.project_root = project_root;
        self.installed = None;
        self.current_turn = None;
        self.active_turn = None;
        BuildStamp::new(self.generation, self.project_root.clone())
    }

    fn build_is_current(&self, stamp: &BuildStamp) -> bool {
        stamp.generation == self.generation && stamp.project_root == self.project_root
    }

    fn install(&mut self, stamp: &BuildStamp) -> bool {
        if !self.build_is_current(stamp) {
            return false;
        }
        self.installed = Some(stamp.clone());
        true
    }

    fn begin_turn(&mut self) -> Option<TurnStamp> {
        let build = self.installed.clone()?;
        if !self.build_is_current(&build) || self.active_turn.is_some() {
            return None;
        }
        self.next_turn = self.next_turn.checked_add(1).expect("agent turn id exhausted");
        let stamp = TurnStamp {
            build,
            turn: TurnId(self.next_turn),
        };
        self.current_turn = Some(stamp.clone());
        self.active_turn = Some(stamp.clone());
        Some(stamp)
    }

    fn finish_turn(&mut self, stamp: &TurnStamp) {
        if self.active_turn.as_ref() == Some(stamp) {
            self.active_turn = None;
        }
    }

    fn turn_to_stop(&self) -> Option<&TurnStamp> {
        let active = self.active_turn.as_ref()?;
        (self.current_turn.as_ref() == Some(active) && self.build_is_current(&active.build))
            .then_some(active)
    }

    fn accepts_event(&self, stamp: &AgentEventStamp) -> bool {
        if !self.build_is_current(&stamp.build) {
            return false;
        }
        match stamp.turn {
            Some(turn) => {
                self.current_turn
                    .as_ref()
                    .is_some_and(|current| current.build == stamp.build && current.turn == turn)
            }
            None => true,
        }
    }
}

#[derive(Clone)]
struct InstalledSession {
    stamp: BuildStamp,
    session: Arc<tokio::sync::Mutex<Session>>,
    max_seconds: u64,
    events: AgentEventSender,
    target: PersistenceTarget,
    quiescence: SessionQuiescence,
}

struct LifecycleResources {
    identity: LifecycleIdentity,
    installed: Option<InstalledSession>,
    active_stopper: Option<(TurnStamp, smithy_agent::StopLease)>,
    building: Option<(BuildStamp, tokio_util::sync::CancellationToken)>,
}

/// The generation-safe session slot.
///
/// The outer mutex is synchronous and held only while cloning handles. The
/// session itself has its own async mutex, which a turn may hold for minutes.
/// That split is what lets a reconnect detach the old slot immediately instead
/// of queuing behind the turn it is trying to stop.
#[derive(Clone)]
pub struct AgentLifecycle {
    inner: Arc<Mutex<LifecycleResources>>,
    save_blocked: Arc<AtomicBool>,
}

struct TurnLease {
    stamp: TurnStamp,
    session: Arc<tokio::sync::Mutex<Session>>,
    events: AgentEventSender,
    save: Option<SaveTarget>,
    control: smithy_agent::ExecutionControl,
    quiescence: TurnQuiescenceGuard,
}

/// Release reconnect's wait even if a turn task unwinds unexpectedly.
///
/// The ordinary path releases explicitly after snapshotting. The drop path
/// prevents a provider/tool panic from leaving every later reconnect waiting
/// forever on a turn whose Session lock has already been released.
struct TurnQuiescenceGuard(SessionQuiescence);

impl Drop for TurnQuiescenceGuard {
    fn drop(&mut self) {
        self.0.finish_turn();
    }
}

impl AgentLifecycle {
    #[cfg(test)]
    fn new(project_root: PathBuf) -> Self {
        Self::new_with_save_guard(project_root, Arc::new(AtomicBool::new(false)))
    }

    fn new_with_save_guard(project_root: PathBuf, save_blocked: Arc<AtomicBool>) -> Self {
        Self {
            inner: Arc::new(Mutex::new(LifecycleResources {
                identity: LifecycleIdentity::new(project_root),
                installed: None,
                active_stopper: None,
                building: None,
            })),
            save_blocked,
        }
    }

    fn transition(&self, project_root: PathBuf) -> (BuildStamp, Option<smithy_agent::StopLease>) {
        let (stamp, stopper, obsolete_build) = {
            let mut state = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            let stamp = state.identity.transition(project_root);
            let stopper = state.active_stopper.take().map(|(_, stopper)| stopper);
            if let Some(installed) = state.installed.take() {
                installed.events.retire();
            }
            let obsolete_build = state.building.take().map(|(_, control)| control);
            (stamp, stopper, obsolete_build)
        };
        // Build tasks are not aborted: doing so detaches an already-running
        // spawn_blocking worker. Cancellation makes scans stop cooperatively;
        // read-only keychain/kernel calls that cannot be interrupted are awaited
        // and their result is quarantined by the retired generation.
        if let Some(control) = obsolete_build {
            control.cancel();
        }
        (stamp, stopper)
    }

    fn register_build(
        &self,
        stamp: &BuildStamp,
        control: tokio_util::sync::CancellationToken,
    ) {
        let obsolete = {
            let mut state = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            if !state.identity.build_is_current(stamp) {
                Some(control)
            } else if state.identity.installed.as_ref() == Some(stamp)
            {
                None
            } else {
                state
                    .building
                    .replace((stamp.clone(), control))
                    .map(|(_, old)| old)
            }
        };
        if let Some(control) = obsolete {
            control.cancel();
        }
    }

    fn finish_build(&self, stamp: &BuildStamp) {
        let mut state = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if state
            .building
            .as_ref()
            .is_some_and(|(building, _)| building == stamp)
        {
            state.building = None;
        }
    }

    fn install(
        &self,
        stamp: &BuildStamp,
        session: Session,
        events: AgentEventSender,
        target: PersistenceTarget,
        usage_cache: &crate::meters::UsageCache,
    ) -> bool {
        let max_seconds = session.limits().max_seconds;
        let usage = session.usage();
        let session = Arc::new(tokio::sync::Mutex::new(session));
        let mut state = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if !state.identity.install(stamp) {
            return false;
        }
        if state
            .building
            .as_ref()
            .is_some_and(|(building, _)| building == stamp)
        {
            state.building = None;
        }
        usage_cache.seed(&session, usage);
        state.installed = Some(InstalledSession {
            stamp: stamp.clone(),
            session,
            max_seconds,
            events,
            target,
            quiescence: SessionQuiescence::default(),
        });
        true
    }

    fn begin_turn(&self) -> Option<TurnLease> {
        if self.save_blocked.load(Ordering::Acquire) {
            return None;
        }
        let mut state = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let stamp = state.identity.begin_turn()?;
        let installed = state.installed.as_ref()?.clone();
        if installed.stamp != stamp.build {
            state.identity.finish_turn(&stamp);
            return None;
        }
        if !installed.quiescence.begin_turn() {
            state.identity.finish_turn(&stamp);
            return None;
        }
        installed.events.activate(stamp.turn);
        let token = smithy_agent::ExecutionToken::new(
            stamp.build.generation.0,
            stamp.turn.0,
        );
        let (control, stopper) = smithy_agent::ExecutionControl::for_turn(
            token,
            std::time::Duration::from_secs(installed.max_seconds),
        );
        state.active_stopper = Some((stamp.clone(), stopper));
        Some(TurnLease {
            stamp,
            session: installed.session,
            events: installed.events,
            save: installed.target.next_save(),
            control,
            quiescence: TurnQuiescenceGuard(installed.quiescence),
        })
    }

    fn finish_turn(
        &self,
        stamp: &TurnStamp,
        events: &AgentEventSender,
        quiescence: &SessionQuiescence,
    ) {
        events.deactivate(stamp.turn);
        let mut state = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        state.identity.finish_turn(stamp);
        if state
            .active_stopper
            .as_ref()
            .is_some_and(|(active, _)| active == stamp)
        {
            state.active_stopper = None;
        }
        drop(state);
        quiescence.finish_turn();
    }

    pub(crate) fn accepts_event(&self, stamp: &AgentEventStamp) -> bool {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .identity
            .accepts_event(stamp)
    }

    pub(crate) fn accepts_review(&self, key: &smithy_editor::ReviewKey) -> bool {
        let state = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let stamp = AgentEventStamp::for_turn(
            BuildStamp::new(
                GenerationId(key.generation),
                state.identity.project_root.clone(),
            ),
            TurnId(key.turn),
        );
        state.identity.accepts_event(&stamp)
    }

    fn build_is_current(&self, stamp: &BuildStamp) -> bool {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .identity
            .build_is_current(stamp)
    }

    /// Stop only the turn that currently owns the visible slot.
    ///
    /// The lease owns the exact generation/turn token. Keeping an old button
    /// callback cannot target a successor because every turn gets a different
    /// cancellation token rather than a rearmed session-global one.
    pub fn stop_current_turn(&self) -> bool {
        let stopper = {
            let state = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            match (state.identity.turn_to_stop(), &state.active_stopper) {
                (Some(active), Some((owned, stopper))) if active == owned => Some(stopper.clone()),
                _ => None,
            }
        };
        match stopper {
            Some(stopper) => {
                stopper.stop();
                true
            }
            None => false,
        }
    }

    pub fn current_session(&self) -> Option<Arc<tokio::sync::Mutex<Session>>> {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .installed
            .as_ref()
            .map(|installed| installed.session.clone())
    }

    fn current_resume(
        &self,
    ) -> Option<(
        Arc<tokio::sync::Mutex<Session>>,
        PersistenceTarget,
        SessionQuiescence,
    )> {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .installed
            .as_ref()
            .map(|installed| {
                (
                    installed.session.clone(),
                    installed.target.clone(),
                    installed.quiescence.clone(),
                )
            })
    }
}

type ResumeCandidate = (
    Arc<tokio::sync::Mutex<Session>>,
    PersistenceTarget,
    SessionQuiescence,
);

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
    /// The generation-safe live session and current-turn stopper.
    pub lifecycle: AgentLifecycle,
    /// The project the agent is grounded in. Changing it rebuilds the session,
    /// because the project description lives in the frozen system prompt.
    pub project: Rc<RefCell<smithy_project::Project>>,
    /// Recent projects and per-project storage layout.
    pub registry: Rc<smithy_project::ProjectRegistry>,
    /// Sessions for the *current* project. Rebuilt when the project changes.
    pub sessions: Rc<RefCell<Option<smithy_agent::SessionStore>>>,
    /// The id of the session currently being appended to.
    pub session_id: Rc<RefCell<String>>,
    /// Whether a build with no installed in-memory target may search disk.
    ///
    /// New Session clears this immediately. Without that bit, reconnecting
    /// before the fresh build installed selected the old newest file again.
    resume_saved_history: Rc<Cell<bool>>,
    /// In-memory history detached by reconnect and retained until a successor
    /// actually installs.
    ///
    /// A failed/aborted provider build must not turn the next Reconnect into a
    /// disk lookup: disk can be one turn behind the session we just detached.
    pending_resume: Rc<RefCell<HashMap<PathBuf, ResumeCandidate>>>,
    /// Failed snapshots coalesced by lineage and retried by one worker each.
    pending_saves: PendingSaveQueue,
    /// Last unresolved save problem, re-shown after transcript rebuilds.
    persistence_status: RwSignal<Option<String>>,
    /// The file explorer, so switching project can re-root it.
    pub file_browser: Rc<RefCell<FileBrowserState>>,
    /// The Problems panel's contents.
    ///
    /// Held here so `switch_project` can empty it. Diagnostics are keyed by file
    /// and replaced per file, never merged — but nothing replaces a file that the
    /// new project simply does not have, so switching left the previous project's
    /// problems on screen and the next project's were *added* to them.
    pub diagnostics: DiagnosticsState,
    /// The hosted account's balance, refreshed on a slow timer by
    /// [`crate::meters`].
    ///
    /// Shared state rather than a local in `main` so a provider switch can clear
    /// it: a figure carried over from the previous account is worse than showing
    /// none at all.
    pub balance: crate::meters::BalanceCache,
    /// Seeded at installation so a turn that immediately owns the session lock
    /// cannot make restored spend flash to zero.
    pub usage_cache: crate::meters::UsageCache,
    /// The project map rendered behind an empty editor.
    ///
    /// Here rather than in `main` so `switch_project` can rebuild it — a map of
    /// the previous repository behind the new one's empty pane would be worse
    /// than no map at all.
    pub project_map: RwSignal<String>,
    /// The call graph shown in the center pane (Benzi-style map).
    pub call_graph: crate::call_graph::CallGraphUi,
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

    let save_blocked = Arc::new(AtomicBool::new(false));
    let lifecycle =
        AgentLifecycle::new_with_save_guard(project.root.clone(), save_blocked.clone());
    let pending_saves = PendingSaveQueue::new(save_blocked);
    let agent = AgentState {
        panel,
        tx: agent_tx,
        tick: agent_tick,
        inbox: agent_inbox,
        lifecycle,
        project: Rc::new(RefCell::new(project)),
        registry: Rc::new(registry),
        sessions: Rc::new(RefCell::new(sessions)),
        session_id: Rc::new(RefCell::new(new_session_id())),
        resume_saved_history: Rc::new(Cell::new(true)),
        pending_resume: Rc::new(RefCell::new(HashMap::new())),
        pending_saves,
        persistence_status: RwSignal::new(None),
        file_browser: file_browser_state.clone(),
        file_browser_refresh: RwSignal::new(0),
        review: ReviewState::new(),
        shell_approval_tx: shell_tx,
        shell_approval,
        shell_tick,
        shell_inbox,
        file_watcher: Rc::new(RefCell::new(None)),
        terminal_tabs: terminal_tabs.clone(),
        lsp_handle: lsp_handle.clone(),
        diagnostics: DiagnosticsState::new(),
        balance: crate::meters::BalanceCache::new(),
        usage_cache: crate::meters::UsageCache::default(),
        project_map: RwSignal::new(String::new()),
        call_graph: crate::call_graph::CallGraphUi::new(),
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

/// Which source reconnect is allowed to consult.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReconnectSelection {
    Current,
    Disk,
    Fresh,
}

fn reconnect_selection(has_current: bool, may_search_disk: bool) -> ReconnectSelection {
    if has_current {
        ReconnectSelection::Current
    } else if may_search_disk {
        ReconnectSelection::Disk
    } else {
        ReconnectSelection::Fresh
    }
}

/// Connect to the configured provider in the background and build the session.
///
/// Deliberately not blocking startup: a missing or unloaded model should leave
/// you with a working editor and a red dot, not a window that refuses to open.
pub fn connect_agent(agent: &AgentState) {
    let root = agent.project.borrow().root.clone();
    let installed = agent.lifecycle.current_resume();
    if let Some(candidate) = installed.clone() {
        agent
            .pending_resume
            .borrow_mut()
            .insert(root.clone(), candidate);
    }
    let current = installed.or_else(|| agent.pending_resume.borrow().get(&root).cloned());
    let source = match reconnect_selection(current.is_some(), agent.resume_saved_history.get()) {
        ReconnectSelection::Current => {
            let (session, target, quiescence) =
                current.expect("current selection has an installed session");
            crate::agent::ResumeSource::Current {
                session,
                target: Box::new(target),
                quiescence,
            }
        }
        ReconnectSelection::Disk => crate::agent::ResumeSource::Disk,
        ReconnectSelection::Fresh => crate::agent::ResumeSource::Fresh,
    };
    // Used only if compatibility rejects the current target. Establishing it
    // now means a failed mismatch build cannot later reuse and overwrite the
    // old id; an exact reconnect restores the retained target id in Ready.
    let fresh_id = if matches!(&source, crate::agent::ResumeSource::Current { .. }) {
        let id = new_session_id();
        *agent.session_id.borrow_mut() = id.clone();
        id
    } else {
        agent.session_id.borrow().clone()
    };
    let request = session_build_request(agent, fresh_id, source);
    let root = agent.project.borrow().root.clone();
    let stamp = begin_session_transition(agent, root);
    spawn_session(agent, stamp, request);
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
    // Establish the new target before any asynchronous build starts. Reconnect
    // during that build is then Fresh, never a disk lookup for the old newest
    // conversation.
    *agent.session_id.borrow_mut() = new_session_id();
    agent.resume_saved_history.set(false);
    let root = agent.project.borrow().root.clone();
    agent.pending_resume.borrow_mut().remove(&root);
    let request = session_build_request(
        agent,
        agent.session_id.borrow().clone(),
        crate::agent::ResumeSource::Fresh,
    );
    let root = agent.project.borrow().root.clone();
    let stamp = begin_session_transition(agent, root);

    agent.panel.clear();
    spawn_session(agent, stamp, request);
}

/// A detached project-switch transition waiting for the new project state to
/// finish re-rooting.
///
/// `main` owns explorer/LSP/watcher/terminal state, so it performs those moves.
/// This token keeps the agent build in the same transition without pulling that
/// unrelated UI state into this module.
pub struct ProjectSessionTransition {
    stamp: BuildStamp,
}

/// Retire the current session before any project-relative UI state moves.
pub fn begin_project_transition(
    agent: &AgentState,
    project_root: PathBuf,
) -> ProjectSessionTransition {
    agent.resume_saved_history.set(true);
    if let Some(candidate) = agent.lifecycle.current_resume() {
        let old_root = agent.project.borrow().root.clone();
        agent.pending_resume.borrow_mut().insert(old_root, candidate);
    }
    ProjectSessionTransition {
        stamp: begin_session_transition(agent, project_root),
    }
}

/// Start the new project's build after its session store and root are in place.
pub fn finish_project_transition(agent: &AgentState, transition: ProjectSessionTransition) {
    let root = agent.project.borrow().root.clone();
    let source = match agent.pending_resume.borrow().get(&root).cloned() {
        Some((session, target, quiescence)) => crate::agent::ResumeSource::Current {
            session,
            target: Box::new(target),
            quiescence,
        },
        None => crate::agent::ResumeSource::Disk,
    };
    let request = session_build_request(
        agent,
        agent.session_id.borrow().clone(),
        source,
    );
    spawn_session(agent, transition.stamp, request);
}

/// One transition shared by startup, every reconnect, New Session and project
/// switching.
///
/// Identity moves first. That makes every old build and event stale before the
/// old turn is asked to stop or a modal is withdrawn; none of those asynchronous
/// completions can race back into the new panel.
fn begin_session_transition(agent: &AgentState, project_root: PathBuf) -> BuildStamp {
    let (stamp, stopper) = agent.lifecycle.transition(project_root);
    if let Some(stopper) = stopper {
        stopper.stop();
    }

    agent.review.abandon();
    abandon_shell_approvals(agent);

    agent.panel.connected.set(false);
    agent.panel.turn_available.set(false);
    agent.panel.busy.set(false);
    agent.panel.streaming_answer.set(String::new());
    agent.panel.streaming_reasoning.set(String::new());
    agent.panel.model_label.set("connecting…".into());
    stamp
}

fn abandon_shell_approvals(agent: &AgentState) {
    if let Some(request) = agent.shell_approval.get_untracked() {
        request.abandon();
    }
    agent.shell_approval.set(None);
    for request in drain(&agent.shell_inbox) {
        request.abandon();
    }
}

fn session_build_request(
    agent: &AgentState,
    fresh_id: String,
    source: crate::agent::ResumeSource,
) -> crate::agent::SessionBuildRequest {
    crate::agent::SessionBuildRequest {
        store: agent
            .sessions
            .borrow()
            .as_ref()
            .map(|store| store.root().to_path_buf()),
        fresh_id,
        source,
    }
}

/// Build a session in the background and hand it to the UI.
///
/// The single path both connecting and clearing take, so the two cannot drift:
/// the only difference between them is whether a stored conversation is replayed.
fn spawn_session(
    agent: &AgentState,
    stamp: BuildStamp,
    request: crate::agent::SessionBuildRequest,
) {
    let project = agent.project.borrow().clone();
    if project.root != stamp.project_root {
        // A project transition is split around explorer/LSP/watcher re-rooting.
        // If a caller finishes it against a different live root, building would
        // freeze the wrong project into the prompt under a plausible generation.
        return;
    }

    // Read on the UI thread on purpose: this is a small JSON file, and reading
    // it *here* is what makes reconnect pick up a setting the dialog just saved
    // without any signalling between the two.
    let config = smithy_agent::AgentConfig::load(agent.registry.data_dir());

    let events = AgentEventSender::new(agent.tx.clone(), stamp.clone());
    let shell_tx = agent.shell_approval_tx.clone();
    let review = crate::agent::ReviewGate {
        pending: agent.review.pending.clone(),
        responders: agent.review.responders.clone(),
        auto_approve: agent.review.auto_approve.clone(),
    };
    let lifecycle = agent.lifecycle.clone();
    let lifecycle_for_build = lifecycle.clone();
    let stamp_for_build = stamp.clone();
    let usage_cache = agent.usage_cache.clone();
    let build_control = tokio_util::sync::CancellationToken::new();
    let task_control = build_control.clone();

    let _task = tokio_runtime().spawn(async move {
        match crate::agent::build_session(
            project,
            config,
            events.clone(),
            shell_tx,
            review,
            request,
            task_control,
        )
        .await
        {
            Ok(handle) => {
                let model_label = handle.model_label.clone();
                let context_limit = handle.context_limit;
                let context_summary = handle.context_summary.clone();
                let restored = handle.restored.clone();
                let session_id = handle.target.session_id();
                let resume_notice = handle.resume_notice.clone();
                let context_usage = handle.context_usage.clone();
                if lifecycle_for_build.install(
                    &stamp_for_build,
                    handle.session,
                    events.clone(),
                    handle.target,
                    &usage_cache,
                ) {
                    events.send_build(AgentUiEventKind::Ready {
                        model_label,
                        context_limit,
                        context_summary,
                        restored,
                        session_id,
                        resume_notice,
                        context_usage,
                    });
                }
            }
            Err(e) => {
                lifecycle_for_build.finish_build(&stamp_for_build);
                if lifecycle_for_build.build_is_current(&stamp_for_build) {
                    events.send_build(AgentUiEventKind::Unavailable(e));
                }
            }
        }
    });
    lifecycle.register_build(&stamp, build_control);
}

/// Send a task to the agent.
///
/// Returns `false` without consuming any composer state when the visible
/// generation has no free session slot.
pub fn submit_task(agent: &AgentState, task: String) -> bool {
    let Some(lease) = claim_submission(&agent.panel, &agent.lifecycle) else {
        return false;
    };

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
    agent.panel.turn_available.set(false);
    agent.panel.streaming_answer.set(String::new());
    agent.panel.streaming_reasoning.set(String::new());

    // One message, one set of attachments. Carrying them forward would re-send
    // every file on every turn, which is both expensive and wrong — the model
    // already has them in history.
    agent.panel.clear_attachments();

    let task = crate::agent::prepend_review_outcomes(&mut agent.review.outcomes.borrow_mut(), task);

    let turn_stamp = lease.stamp.clone();
    let save = lease.save;
    let lifecycle = agent.lifecycle.clone();
    let turn_event_stamp = lease.stamp.event();
    let events = lease.events.clone();
    let pending_saves = agent.pending_saves.clone();
    let session = lease.session;
    let control = lease.control;
    let quiescence = lease.quiescence;

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
        let attachment_control = control.clone();
        let materialize = tokio::task::spawn_blocking(move || {
            smithy_editor::attachment::materialize_controlled(
                &attachments,
                &task,
                || attachment_control.check(),
            )
        });
        // spawn_blocking cannot be aborted once running. Always observe its
        // completion: the worker checks control between bounded regular-file
        // chunks, so Stop cannot detach an ongoing FIFO/device/unbounded read.
        let task = match materialize.await {
            Ok(Ok(task)) => task,
            Ok(Err(_)) | Err(_) => unattached,
        };

        let mut guard = session.lock().await;
        let terminal = crate::agent::run_turn(
            &mut guard,
            task,
            events.clone(),
            turn_event_stamp.clone(),
            control,
        )
        .await;
        let pending_save = completed_turn_snapshot(save, &guard);
        drop(guard);
        lifecycle.finish_turn(&turn_stamp, &events, &quiescence.0);

        // Persistence belongs to the completed turn, not to whether its UI
        // envelope is still current. A project switch may discard the terminal
        // event below; the old target must still receive its final History.
        persist_completed_turn(pending_save, pending_saves, events.clone()).await;
        events.send_finished(&turn_event_stamp, terminal);
    });
    true
}

/// Claim the app's turn slot before consuming any user-authored draft state.
///
/// The editor callback also guards disconnected sends for immediate feedback,
/// but this is the authority: connectivity can change between a button paint
/// and this call. Keeping the claim ahead of attachment clearing and review
/// outcome draining is what makes rejection lossless at the app boundary.
fn claim_submission(
    panel: &AgentPanelState,
    lifecycle: &AgentLifecycle,
) -> Option<TurnLease> {
    if !panel.connected.get_untracked()
        || !panel.turn_available.get_untracked()
        || panel.busy.get_untracked()
    {
        return None;
    }
    match lifecycle.begin_turn() {
        Some(lease) => Some(lease),
        None => {
            panel.turn_available.set(false);
            None
        }
    }
}

/// Translate agent events into panel state. Runs on the UI thread.
/// Show the next shell approval request whenever the modal is free.
///
/// Separate from the modal itself because a request can arrive while one is
/// already on screen: the modal advances when answered, and this covers the case
/// where the queue was empty at that moment and filled afterwards.
/// Mirror the panel's auto-approve toggle into the flag the write hook reads.
///
/// Two representations of one setting, because they live on different threads:
/// the toggle is a floem signal on the UI thread, and the hook runs on the tokio
/// runtime where floem's reactive graph must not be touched. An `Effect` is the
/// one-way bridge, so the signal stays the source of truth.
pub fn setup_auto_approve_effect(agent: AgentState) {
    let toggle = agent.panel.auto_approve;
    let flag = agent.review.auto_approve.clone();
    floem::reactive::Effect::new(move |_| {
        flag.store(toggle.get(), std::sync::atomic::Ordering::Relaxed);
    });
}

pub fn setup_shell_approval_effect(agent: AgentState) {
    let tick = agent.shell_tick;
    let inbox = agent.shell_inbox.clone();
    let slot = agent.shell_approval;
    let lifecycle = agent.lifecycle;

    floem::reactive::Effect::new(move |_| {
        tick.get();
        if slot.get_untracked().is_none() {
            while let Some(next) = pop(&inbox) {
                if lifecycle.accepts_event(&next.stamp) && next.is_pending() {
                    slot.set(Some(next));
                    break;
                }
                next.abandon();
            }
        }
    });
}

pub fn setup_agent_effect(agent: AgentState) {
    let panel = agent.panel;
    let tick = agent.tick;
    let inbox = agent.inbox.clone();
    let current_diff = agent.review.current;
    let persistence_status = agent.persistence_status;
    let for_save = agent.clone();

    floem::reactive::Effect::new(move |_| {
        tick.get();
        // Every event, not just the last of the batch. Streaming deltas arrive
        // several per frame and the previous arrangement kept only one of them.
        for event in drain(&inbox) {
            if let AgentUiEventKind::Persistence(status) = event.kind.clone() {
                match status {
                    PersistenceStatus::Failed(message) => {
                        persistence_status.set(Some(message.clone()));
                        panel.push(smithy_editor::AgentEntry::Error(message));
                    }
                    PersistenceStatus::Conflict(message) => {
                        persistence_status.set(Some(message.clone()));
                        panel.push(smithy_editor::AgentEntry::Notice(message));
                    }
                    PersistenceStatus::HardFailure(message) => {
                        persistence_status.set(Some(message.clone()));
                        panel.turn_available.set(false);
                        panel.push(smithy_editor::AgentEntry::Error(message));
                    }
                    PersistenceStatus::Recovered => {
                        let prior = persistence_status.get_untracked();
                        if prior.as_ref().is_some_and(|message| {
                            message.starts_with("Session save pending")
                                || message.starts_with("Session save worker")
                        }) {
                            persistence_status.set(None);
                            panel.push(smithy_editor::AgentEntry::Notice(
                                "Session save recovered; retained snapshots are now on disk."
                                    .into(),
                            ));
                        }
                    }
                }
                continue;
            }
            // This check is intentionally before the match. In particular, a
            // stale Ready must not roll `session_id` back, and a stale terminal
            // event must not repaint the current panel. Persistence has already
            // run through the immutable turn target before this UI envelope.
            if !for_save.lifecycle.accepts_event(&event.stamp) {
                continue;
            }
            match event.kind {
                AgentUiEventKind::Ready {
                    model_label,
                    context_limit,
                    context_summary,
                    restored,
                    session_id,
                    resume_notice,
                    context_usage,
                } => {
                    *for_save.session_id.borrow_mut() = session_id;
                    for_save.resume_saved_history.set(false);
                    let root = for_save.project.borrow().root.clone();
                    for_save.pending_resume.borrow_mut().remove(&root);
                    if resume_notice.is_some() {
                        panel.clear();
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
                    if let Some(notice) = resume_notice {
                        panel.push(smithy_editor::AgentEntry::Notice(notice));
                    }
                    if let Some(status) = persistence_status.get_untracked() {
                        panel.push(smithy_editor::AgentEntry::Notice(status));
                    }
                    // Always say so in the transcript. The header label changes
                    // quietly enough that a Save & reconnect can look like it
                    // did nothing — this is the receipt.
                    panel.push(smithy_editor::AgentEntry::Notice(format!(
                        "Connected · {model_label}"
                    )));
                    panel.model_label.set(model_label);
                    panel.context_limit.set(context_limit);
                    panel.context_label.set(context_summary);
                    match context_usage {
                        Some((prompt_tokens, snapshot)) => {
                            panel.context_tokens.set(prompt_tokens);
                            panel.context_usage.set(Some(snapshot));
                        }
                        None => {
                            panel.context_tokens.set(0);
                            panel.context_usage.set(None);
                        }
                    }
                    panel.connected.set(true);
                    panel.turn_available.set(
                        !for_save.lifecycle.save_blocked.load(Ordering::Acquire),
                    );
                }
                AgentUiEventKind::Unavailable(reason) => {
                    panel.connected.set(false);
                    panel.turn_available.set(false);
                    panel.model_label.set("disconnected".into());
                    panel.busy.set(false);
                    panel.push(smithy_editor::AgentEntry::Error(reason));
                }
                AgentUiEventKind::Turn(turn) => apply_turn_event(&panel, turn),
                AgentUiEventKind::ReviewRequested(diff) => {
                    // Raise the modal only if nothing is already under review; the
                    // rest queue behind it and surface as each is resolved.
                    if current_diff.get_untracked().is_none() {
                        current_diff.set(Some(diff));
                    }
                }
                AgentUiEventKind::Answered(answer) => {
                    panel.streaming_answer.set(String::new());
                    panel.streaming_reasoning.set(String::new());
                    panel.push(smithy_editor::AgentEntry::Answer(answer));
                    panel.busy.set(false);
                    panel.turn_available.set(
                        !for_save.lifecycle.save_blocked.load(Ordering::Acquire),
                    );
                }
                AgentUiEventKind::Stopped(reason) => {
                    panel.streaming_answer.set(String::new());
                    panel.streaming_reasoning.set(String::new());
                    panel.push(smithy_editor::AgentEntry::Stopped(reason));
                    panel.busy.set(false);
                    panel.turn_available.set(
                        !for_save.lifecycle.save_blocked.load(Ordering::Acquire),
                    );
                }
                AgentUiEventKind::Failed(error) => {
                    panel.streaming_answer.set(String::new());
                    panel.streaming_reasoning.set(String::new());
                    panel.push(smithy_editor::AgentEntry::Error(error));
                    panel.busy.set(false);
                    panel.turn_available.set(
                        !for_save.lifecycle.save_blocked.load(Ordering::Acquire),
                    );
                }
                AgentUiEventKind::Persistence(_) => unreachable!(
                    "persistence status is handled before generation filtering"
                ),
                AgentUiEventKind::ContextUsage {
                    prompt_tokens,
                    snapshot,
                } => {
                    // Stash once per completion — budget_bar only reads this.
                    // Computing ledger (or walking tools/history) inside
                    // Label::derived would re-run at paint rate. That exact
                    // mistake has been paid for twice here: an unconditional
                    // `signal.set` from a paint path, and `CallGraph::staleness`
                    // computed while painting. Both hung the window.
                    panel.context_tokens.set(prompt_tokens);
                    panel.context_usage.set(Some(snapshot));
                }
            }
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
        TranscriptEntry::Stopped(text) => AgentEntry::Stopped(text),
        TranscriptEntry::Failed(text) => AgentEntry::Error(text),
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

    struct AlwaysAnswers;

    #[async_trait::async_trait]
    impl smithy_agent::Provider for AlwaysAnswers {
        fn name(&self) -> &str {
            "answering-test-provider"
        }

        fn model(&self) -> &str {
            "answering-test-model"
        }

        async fn complete(
            &self,
            _request: smithy_agent::CompletionRequest<'_>,
            _on_delta: Option<&(dyn Fn(smithy_agent::Delta) + Send + Sync)>,
        ) -> Result<smithy_agent::Completion, smithy_agent::ProviderError> {
            Ok(smithy_agent::Completion {
                content: "persisted answer".into(),
                finish_reason: "stop".into(),
                prompt_tokens: 42,
                completion_tokens: 3,
                ..Default::default()
            })
        }
    }

    struct AlwaysFails;

    #[async_trait::async_trait]
    impl smithy_agent::Provider for AlwaysFails {
        fn name(&self) -> &str {
            "failing-test-provider"
        }

        fn model(&self) -> &str {
            "failing-test-model"
        }

        async fn complete(
            &self,
            _request: smithy_agent::CompletionRequest<'_>,
            _on_delta: Option<&(dyn Fn(smithy_agent::Delta) + Send + Sync)>,
        ) -> Result<smithy_agent::Completion, smithy_agent::ProviderError> {
            Err(smithy_agent::ProviderError::Other(
                "provider exploded".into(),
            ))
        }
    }

    fn failing_session(root: &std::path::Path) -> Session {
        let workspace = smithy_tools::Workspace::open(root).unwrap();
        Session::new(
            Arc::new(AlwaysFails),
            Arc::new(smithy_tools::Registry::core()),
            Arc::new(smithy_tools::ToolCtx::new(workspace)),
            smithy_agent::SessionConfig::new("test system"),
        )
    }

    fn answering_session(root: &std::path::Path) -> Session {
        let workspace = smithy_tools::Workspace::open(root).unwrap();
        Session::new(
            Arc::new(AlwaysAnswers),
            Arc::new(smithy_tools::Registry::core()),
            Arc::new(smithy_tools::ToolCtx::new(workspace)),
            smithy_agent::SessionConfig::new("test system"),
        )
    }

    fn test_binding() -> smithy_agent::persist::SessionBinding {
        smithy_agent::persist::SessionBinding::new(
            "local",
            "http://localhost:1234/v1",
            "failing-test-model",
            None,
            &smithy_tools::Registry::core().openai_schemas(),
            std::env::temp_dir().as_path(),
        )
        .unwrap()
    }

    async fn run_and_persist_test_turn(
        sessions: &std::path::Path,
        workspace: &std::path::Path,
        id: &str,
        mut session: Session,
        stop_before_start: bool,
    ) -> (
        AgentUiEventKind,
        smithy_agent::persist::StoredSession,
    ) {
        let target = PersistenceTarget::new(
            Some(sessions.to_path_buf()),
            id.into(),
            workspace.to_path_buf(),
            "test-model".into(),
            test_binding(),
            0,
        );
        let save = target.next_save();
        let (control, stopper) = session.control_for_turn(
            smithy_agent::ExecutionToken::new(1, 1),
        );
        if stop_before_start {
            stopper.stop();
        }
        let (tx, _rx) = unbounded();
        let events = AgentEventSender::new(
            tx,
            BuildStamp::new(GenerationId::test(1), workspace.to_path_buf()),
        );
        events.activate(TurnId::test(1));
        let stamp = events.active_stamp().unwrap();
        let terminal = crate::agent::run_turn(
            &mut session,
            id.to_string(),
            events.clone(),
            stamp,
            control,
        )
        .await;
        let pending = completed_turn_snapshot(save, &session);
        persist_completed_turn(pending, PendingSaveQueue::default(), events).await;
        let stored = smithy_agent::SessionStore::new(sessions.to_path_buf())
            .unwrap()
            .load(id)
            .unwrap();
        (terminal, stored)
    }

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
        let root = tempfile::tempdir().unwrap();
        let workspace = smithy_tools::Workspace::open(root.path()).unwrap();

        let change = smithy_editor::PendingFileChange::new(
            smithy_editor::ReviewKey::new(1, 1, "call_1"),
            workspace.identity().clone(),
            PathBuf::from("README.md"),
            smithy_tools::FileSnapshot::Present(smithy_tools::FileBase {
                content: "old\n".to_string(),
                identity: None,
            }),
            "new\n".to_string(),
            "written".into(),
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

    /// The panel guard is not enough: connectivity can retire after a click but
    /// before `submit_task` runs. The app boundary must claim a real turn before
    /// it clears attachments or lets the composer clear its text, and both a
    /// disconnected panel and a missing session slot must reject losslessly.
    #[test]
    fn rejected_app_submissions_preserve_the_composer_and_attachments() {
        let panel = AgentPanelState::new();
        panel.input.set("keep the draft".into());
        panel.attachments.set(vec![smithy_editor::attachment::Attachment {
            path: PathBuf::from("notes.txt"),
            display: "notes.txt".into(),
            bytes: 12,
            kind: smithy_editor::attachment::AttachmentKind::Text,
            included: true,
        }]);
        panel.turn_available.set(true);
        let lifecycle = AgentLifecycle::new(PathBuf::from("/project"));

        assert!(
            claim_submission(&panel, &lifecycle).is_none(),
            "disconnected submission must be rejected"
        );
        panel.connected.set(true);
        assert!(
            claim_submission(&panel, &lifecycle).is_none(),
            "a connected label without an installed session must still be rejected"
        );

        assert_eq!(panel.input.get_untracked(), "keep the draft");
        assert_eq!(panel.attachments.get_untracked().len(), 1);
    }

    /// New Session can be followed by Reconnect before its async build installs.
    /// The old implementation saw no live session and blindly loaded the newest
    /// disk file, resurrecting the conversation the user had just left.
    #[test]
    fn new_session_reconnect_stays_fresh_until_a_current_target_exists() {
        assert_eq!(
            reconnect_selection(false, false),
            ReconnectSelection::Fresh,
            "New Session has established a fresh target, so disk is ineligible"
        );
        assert_eq!(
            reconnect_selection(true, false),
            ReconnectSelection::Current,
            "once installed, reconnect must use the current in-memory history"
        );
        assert_eq!(
            reconnect_selection(false, true),
            ReconnectSelection::Disk,
            "startup and project switch may still discover compatible history"
        );
    }

    /// A claimed turn reserves its save revision before attachment materializing
    /// acquires the Session lock. Reconnect once won that lock first and replayed
    /// the pre-turn snapshot, permanently skipping the submitted turn. Its
    /// in-memory replay must wait for the claimed turn's snapshot boundary.
    #[tokio::test]
    async fn reconnect_waits_for_a_claimed_turn_before_snapshotting_memory() {
        let quiescence = SessionQuiescence::default();
        assert!(quiescence.begin_turn());
        let waiting = quiescence.clone();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (finished_tx, mut finished_rx) = tokio::sync::oneshot::channel();
        let waiter = tokio::spawn(async move {
            let _ = started_tx.send(());
            waiting.wait_until_idle().await;
            let _ = finished_tx.send(());
        });

        started_rx.await.unwrap();
        tokio::task::yield_now().await;
        assert!(
            finished_rx.try_recv().is_err(),
            "reconnect passed the barrier while the turn still owned its revision"
        );

        quiescence.finish_turn();
        finished_rx.await.unwrap();
        waiter.await.unwrap();
    }

    /// Switching away and back can rebuild a compatible target while a retired
    /// turn's save is still in flight. Independent counters issued the same
    /// revision to both branches, making one completed turn disappear.
    #[test]
    fn rebuilt_targets_continue_after_revisions_allocated_but_not_yet_saved() {
        let temp = tempfile::tempdir().unwrap();
        let store = temp.path().join("sessions");
        let workspace = temp.path().join("workspace");
        let first = PersistenceTarget::new(
            Some(store.clone()),
            "shared-session".into(),
            workspace.clone(),
            "model".into(),
            test_binding(),
            7,
        );
        let retired_save = first.next_save().unwrap();
        assert_eq!(retired_save.revision, 8);

        // Disk can still say seven because the retired save has not renamed.
        let rebuilt = PersistenceTarget::new(
            Some(store),
            "shared-session".into(),
            workspace,
            "model".into(),
            test_binding(),
            7,
        );
        assert_eq!(rebuilt.revision(), 8);
        assert_eq!(rebuilt.next_save().unwrap().revision, 9);
    }

    /// Saving only successful answers hid stopped turns after relaunch, while
    /// putting stop metadata into History would change the replay prefix. Both
    /// terminal states must survive in the sidecar and keep History truthful.
    #[tokio::test]
    async fn answered_and_stopped_turns_are_persisted_beside_history() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        let sessions = temp.path().join("sessions");
        std::fs::create_dir_all(&workspace).unwrap();

        let (answered, answered_stored) = run_and_persist_test_turn(
            &sessions,
            &workspace,
            "answered-turn",
            answering_session(&workspace),
            false,
        )
        .await;
        assert!(matches!(answered, AgentUiEventKind::Answered(_)));
        assert_eq!(
            answered_stored
                .turn_outcomes
                .last()
                .map(|entry| entry.status),
            Some(smithy_agent::persist::PersistedTurnStatus::Answered)
        );
        assert!(
            answered_stored
                .messages
                .iter()
                .any(|message| message.content == "persisted answer")
        );

        let (stopped, stopped_stored) = run_and_persist_test_turn(
            &sessions,
            &workspace,
            "stopped-turn",
            answering_session(&workspace),
            true,
        )
        .await;
        assert!(matches!(stopped, AgentUiEventKind::Stopped(_)));
        let outcome = stopped_stored.turn_outcomes.last().unwrap();
        assert_eq!(
            outcome.status,
            smithy_agent::persist::PersistedTurnStatus::Stopped
        );
        assert_eq!(outcome.detail.as_deref(), Some(smithy_agent::CANCELLED));
        assert!(
            stopped_stored
                .messages
                .iter()
                .any(|message| message.content == "stopped-turn"),
            "the stopped turn's user message disappeared from append-only History"
        );
        assert!(
            !stopped_stored
                .messages
                .iter()
                .any(|message| message.content == smithy_agent::CANCELLED),
            "stop metadata entered provider-visible History"
        );
    }

    /// A project switch retires the UI generation before the old provider task
    /// unwinds. Persistence used to live in the UI terminal handler, so that
    /// stale event was discarded together with the only save of a failed turn.
    /// The immutable target must save both History and failure sidecar anyway.
    #[tokio::test]
    async fn a_failed_retired_turn_persists_even_when_its_ui_event_is_stale() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        let sessions = temp.path().join("sessions");
        std::fs::create_dir_all(&workspace).unwrap();

        let target = PersistenceTarget::new(
            Some(sessions.clone()),
            "failed-turn".into(),
            workspace.clone(),
            "failing-test-model".into(),
            test_binding(),
            0,
        );
        let save = target.next_save();
        let mut session = failing_session(&workspace);
        let (tx, _rx) = unbounded();
        let events = AgentEventSender::new(
            tx,
            BuildStamp::new(GenerationId::test(1), workspace.clone()),
        );
        events.activate(TurnId::test(1));
        let stamp = events.active_stamp().unwrap();
        let (control, _) =
            session.control_for_turn(smithy_agent::ExecutionToken::new(1, 1));
        let terminal = crate::agent::run_turn(
            &mut session,
            "please fail".into(),
            events.clone(),
            stamp,
            control,
        )
        .await;
        assert!(matches!(terminal, AgentUiEventKind::Failed(_)));

        let pending = completed_turn_snapshot(save, &session);
        events.retire();
        assert!(
            events.active_stamp().is_none(),
            "the terminal UI route is stale before persistence"
        );

        persist_completed_turn(pending, PendingSaveQueue::default(), events).await;

        let stored = smithy_agent::SessionStore::new(sessions)
            .unwrap()
            .load("failed-turn")
            .unwrap();
        assert!(
            stored
                .messages
                .iter()
                .any(|message| message.content == "please fail"),
            "the failed turn's append-only History was dropped"
        );
        assert_eq!(
            stored.turn_outcomes.last().map(|entry| entry.status),
            Some(smithy_agent::persist::PersistedTurnStatus::Failed)
        );
        let failure = stored
            .turn_outcomes
            .last()
            .and_then(|entry| entry.failure.as_ref())
            .expect("failed turns carry a structured failure");
        assert_eq!(
            failure.category,
            smithy_agent::persist::PersistedFailureCategory::Provider
        );
        assert!(!serde_json::to_string(failure)
            .unwrap()
            .contains("provider exploded"));
        assert!(
            !stored
                .messages
                .iter()
                .any(|message| message.content.contains("provider exploded")),
            "failure metadata entered provider-visible History"
        );
    }

    /// Three connection attempts can complete in any order: a slow A or B must
    /// not replace C after C has already connected. The old slot had no identity,
    /// so whichever async build acquired its mutex last became the live session.
    #[test]
    fn only_the_newest_of_out_of_order_builds_can_install() {
        let mut lifecycle = LifecycleIdentity::new(PathBuf::from("/a"));
        let a = lifecycle.transition(PathBuf::from("/a"));
        let b = lifecycle.transition(PathBuf::from("/b"));
        let c = lifecycle.transition(PathBuf::from("/c"));

        assert!(!lifecycle.install(&b), "B retired when C began");
        assert!(lifecycle.install(&c), "the current build must install");
        assert!(!lifecycle.install(&a), "a very late A must not replace C");
        assert_eq!(lifecycle.installed.as_ref(), Some(&c));
    }

    /// Generation checks prevent installation but do not stop the work itself.
    /// A superseded build can still probe a provider and scan two full source
    /// trees unless its retained control is cancelled by the transition.
    #[tokio::test]
    async fn a_superseded_build_task_is_cancelled_promptly() {
        let lifecycle = AgentLifecycle::new(PathBuf::from("/project"));
        let (old, _) = lifecycle.transition(PathBuf::from("/project"));
        let control = tokio_util::sync::CancellationToken::new();
        let task_control = control.clone();
        let task = tokio::spawn(async move {
            task_control.cancelled().await;
        });
        lifecycle.register_build(&old, control);

        lifecycle.transition(PathBuf::from("/project"));

        task.await.expect("the obsolete build observed cancellation");
    }

    /// Aborting only the async wrapper detached a symbol-index spawn_blocking
    /// worker after it had started. A transition now cancels the real walk and
    /// retains the async observer until the worker reports completion.
    #[tokio::test]
    async fn superseding_after_a_real_symbol_worker_starts_observes_its_exit() {
        let temp = tempfile::tempdir().unwrap();
        for index in 0..200 {
            std::fs::write(
                temp.path().join(format!("file_{index}.rs")),
                format!("pub fn symbol_{index}() {{}}\n"),
            )
            .unwrap();
        }
        let lifecycle = AgentLifecycle::new(temp.path().to_path_buf());
        let (old, _) = lifecycle.transition(temp.path().to_path_buf());
        let control = tokio_util::sync::CancellationToken::new();
        let worker_control = control.clone();
        let root = temp.path().to_path_buf();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let started_tx = Arc::new(Mutex::new(Some(started_tx)));
        let worker = tokio::task::spawn_blocking(move || {
            smithy_project::symbols::SymbolIndex::build_controlled(&root, || {
                if let Ok(mut sender) = started_tx.lock() {
                    if let Some(sender) = sender.take() {
                        let _ = sender.send(());
                    }
                }
                worker_control.is_cancelled()
            })
        });
        let task = tokio::spawn(async move { worker.await.unwrap() });
        lifecycle.register_build(&old, control);

        started_rx.await.expect("the real file walk started");
        lifecycle.transition(temp.path().to_path_buf());

        assert!(
            task.await.unwrap().is_none(),
            "the retired index completed instead of observing cancellation"
        );
    }

    /// Generation equality alone is not enough: a project switch changes the
    /// frozen project prompt, and installing a build made for another root would
    /// give the panel a plausible-looking session grounded in the wrong tree.
    #[test]
    fn a_build_for_the_wrong_root_cannot_install() {
        let mut lifecycle = LifecycleIdentity::new(PathBuf::from("/old"));
        let current = lifecycle.transition(PathBuf::from("/wanted"));
        let wrong_root = BuildStamp::new(current.generation, PathBuf::from("/other"));

        assert!(!lifecycle.install(&wrong_root));
        assert!(lifecycle.installed.is_none());
    }

    /// New Session used to clear only the panel while the old async session
    /// remained reachable. Its History could then answer into the empty
    /// transcript. A same-root transition must detach both the installed slot
    /// and the turn that owned it; it must never mutate or truncate History.
    #[test]
    fn new_session_detaches_the_old_history_and_turn() {
        let root = PathBuf::from("/project");
        let mut lifecycle = LifecycleIdentity::new(root.clone());
        let old = lifecycle.transition(root.clone());
        assert!(lifecycle.install(&old));
        assert!(lifecycle.begin_turn().is_some());

        let fresh = lifecycle.transition(root);

        assert_ne!(fresh.generation, old.generation);
        assert!(lifecycle.installed.is_none());
        assert!(lifecycle.current_turn.is_none());
        assert!(lifecycle.turn_to_stop().is_none());
    }

    /// Ready can replace the transcript/session id; turn and terminal events can
    /// append text or clear busy state. Once B begins, none of A's variants may
    /// be accepted even if already queued. Persistence is deliberately outside
    /// this UI gate and covered by the retired-failure test above.
    #[test]
    fn stale_ready_turn_and_terminal_events_are_rejected_together() {
        let mut lifecycle = LifecycleIdentity::new(PathBuf::from("/a"));
        let a = lifecycle.transition(PathBuf::from("/a"));
        assert!(lifecycle.install(&a));
        let a_turn = lifecycle.begin_turn().expect("A turn");
        let a_ready = AgentEventStamp::for_build(a.clone());
        let a_event = a_turn.event();

        let b = lifecycle.transition(PathBuf::from("/b"));

        assert!(!lifecycle.accepts_event(&a_ready));
        assert!(!lifecycle.accepts_event(&a_event));
        assert!(lifecycle.install(&b));
        assert!(lifecycle.accepts_event(&AgentEventStamp::for_build(b)));
    }

    /// Switching projects while a turn is running used to leave that turn
    /// writing into the shared panel after explorer/LSP/watcher/terminal state
    /// had moved. The root transition must retire the visible turn immediately,
    /// before the old task has actually unwound.
    #[test]
    fn switching_projects_retires_the_running_turn_immediately() {
        let mut lifecycle = LifecycleIdentity::new(PathBuf::from("/first"));
        let first = lifecycle.transition(PathBuf::from("/first"));
        assert!(lifecycle.install(&first));
        let turn = lifecycle.begin_turn().expect("running turn");

        lifecycle.transition(PathBuf::from("/second"));

        assert!(lifecycle.turn_to_stop().is_none());
        assert!(!lifecycle.accepts_event(&turn.event()));
        assert!(lifecycle.installed.is_none());
    }

    /// Stop ownership is an app-level turn identity. Finishing turn one removes
    /// that ownership, and a delayed finish from one must not remove turn two's
    /// ownership. Session's internal token-rearm window is covered by the
    /// dedicated cancellation todo, not this identity test.
    #[test]
    fn stop_targets_only_the_current_visible_turn() {
        let mut lifecycle = LifecycleIdentity::new(PathBuf::from("/project"));
        let build = lifecycle.transition(PathBuf::from("/project"));
        assert!(lifecycle.install(&build));
        let first = lifecycle.begin_turn().expect("first turn");
        assert_eq!(lifecycle.turn_to_stop(), Some(&first));

        lifecycle.finish_turn(&first);
        assert!(lifecycle.turn_to_stop().is_none());

        let second = lifecycle.begin_turn().expect("second turn");
        lifecycle.finish_turn(&first);
        assert_eq!(
            lifecycle.turn_to_stop(),
            Some(&second),
            "a late completion from turn one must not disarm turn two"
        );
    }

    /// Generation checks alone cannot distinguish two turns in one session. If
    /// a delayed terminal or review from turn one is accepted after turn two
    /// starts, it can clear turn two's busy state or raise turn one's modal over
    /// it. The turn id makes the older event stale without rebuilding History.
    #[test]
    fn an_event_from_the_previous_turn_cannot_touch_the_next_one() {
        let mut lifecycle = LifecycleIdentity::new(PathBuf::from("/project"));
        let build = lifecycle.transition(PathBuf::from("/project"));
        assert!(lifecycle.install(&build));
        let first = lifecycle.begin_turn().expect("first turn");
        lifecycle.finish_turn(&first);
        let second = lifecycle.begin_turn().expect("second turn");

        assert!(!lifecycle.accepts_event(&first.event()));
        assert!(lifecycle.accepts_event(&second.event()));
    }

    /// A queued review is a turn event with a longer lifetime. Checking only its
    /// opaque responder id at Apply time let an old generation's modal mutate a
    /// successor session after reconnect.
    #[test]
    fn a_review_from_the_wrong_generation_or_turn_is_rejected() {
        let root = PathBuf::from("/project");
        let lifecycle = AgentLifecycle::new(root.clone());
        let (generation, turn) = {
            let mut state = lifecycle.inner.lock().unwrap();
            let build = state.identity.transition(root);
            assert!(state.identity.install(&build));
            let turn = state.identity.begin_turn().unwrap();
            (build.generation.0, turn.turn.0)
        };

        assert!(lifecycle.accepts_review(&smithy_editor::ReviewKey::new(
            generation,
            turn,
            "current"
        )));
        assert!(!lifecycle.accepts_review(&smithy_editor::ReviewKey::new(
            generation + 1,
            turn,
            "wrong-generation"
        )));
        assert!(!lifecycle.accepts_review(&smithy_editor::ReviewKey::new(
            generation,
            turn + 1,
            "wrong-turn"
        )));
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

    fn pending_history_snapshot(
        target: &PersistenceTarget,
        history: &smithy_agent::History,
    ) -> (SaveTarget, smithy_agent::persist::StoredSession) {
        let save = target.next_save().unwrap();
        let stored = smithy_agent::persist::StoredSession::from_session_state(
            save.session_id.clone(),
            &save.project_root,
            &save.configured_model,
            save.binding.clone(),
            save.revision,
            history,
            &smithy_agent::Sampling::default(),
            &smithy_agent::Limits::default(),
            Vec::new(),
            Vec::new(),
            smithy_agent::SessionAccounting::default(),
        );
        (save, stored)
    }

    /// A permanently unwritable store used to retain one full transcript and
    /// spawn one retry task per turn. One hundred append-only updates must
    /// coalesce into one latest snapshot, wake the sleeping worker, and recover
    /// by writing only the complete newest history.
    #[tokio::test]
    async fn one_hundred_failed_updates_keep_one_snapshot_and_one_worker() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        let sessions = temp.path().join("sessions");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(&sessions, "blocks directory creation").unwrap();
        let target = PersistenceTarget::new(
            Some(sessions.clone()),
            "retry".into(),
            workspace.clone(),
            "test-model".into(),
            test_binding(),
            0,
        );
        let queue = PendingSaveQueue::default();
        let (tx, rx) = unbounded();
        let events = AgentEventSender::new(
            tx,
            BuildStamp::new(GenerationId::test(1), workspace.clone()),
        );
        let mut history = smithy_agent::History::new("test system");

        for turn in 1..=100 {
            history.push(smithy_agent::Message::user(format!("turn-{turn}")));
            persist_completed_turn(
                Some(pending_history_snapshot(&target, &history)),
                queue.clone(),
                events.clone(),
            )
            .await;
        }
        assert!(matches!(
            rx.recv_timeout(std::time::Duration::from_secs(1))
                .unwrap()
                .kind,
            AgentUiEventKind::Persistence(PersistenceStatus::Failed(_))
        ));
        let wake = {
            let state = queue.inner.lock().unwrap();
            assert_eq!(state.lineages.len(), 1);
            let lineage = state.lineages.values().next().unwrap();
            assert_eq!(lineage.pending.stored.revision, 100);
            assert_eq!(lineage.pending.stored.messages.len(), 101);
            lineage.wake.clone()
        };
        assert_eq!(
            queue.workers_started.load(Ordering::Relaxed),
            1,
            "append-compatible updates spawned duplicate retry workers"
        );

        std::fs::remove_file(&sessions).unwrap();
        // Deterministically wake the worker while it is in capped backoff.
        wake.notify_one();
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if !queue.has_pending() {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("the autonomous retry never ran");
        let stored = smithy_agent::SessionStore::new(sessions)
            .unwrap()
            .load("retry")
            .unwrap();
        assert_eq!(stored.revision, 100);
        assert_eq!(stored.messages.last().unwrap().content, "turn-100");
    }

    /// Divergent histories cannot coalesce. They receive independent bounded
    /// lineages until the safety ceiling hard-stops turns; the already-completing
    /// overflow snapshot is retained in the one emergency slot, never dropped.
    #[test]
    fn divergent_pending_branches_hit_a_hard_bounded_unsaved_state() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        let sessions = temp.path().join("sessions");
        std::fs::create_dir_all(&workspace).unwrap();
        let hard_stop = Arc::new(AtomicBool::new(false));
        let queue = PendingSaveQueue::new(hard_stop.clone());
        let target = PersistenceTarget::new(
            Some(sessions),
            "branch-root".into(),
            workspace.clone(),
            "test-model".into(),
            test_binding(),
            0,
        );
        for branch in 0..MAX_PENDING_LINEAGES {
            let mut history = smithy_agent::History::new("test system");
            history.push(smithy_agent::Message::assistant(format!("branch-{branch}")));
            assert!(matches!(
                queue.enqueue(pending_history_snapshot(&target, &history)),
                QueueAction::Start { .. }
            ));
        }
        assert!(hard_stop.load(Ordering::Acquire));
        assert_eq!(
            queue.inner.lock().unwrap().lineages.len(),
            MAX_PENDING_LINEAGES
        );

        let mut overflow = smithy_agent::History::new("test system");
        overflow.push(smithy_agent::Message::assistant("overflow branch"));
        assert!(matches!(
            queue.enqueue(pending_history_snapshot(&target, &overflow)),
            QueueAction::Hard(_)
        ));
        let state = queue.inner.lock().unwrap();
        assert_eq!(state.lineages.len(), MAX_PENDING_LINEAGES);
        assert!(state.hard_unsaved.is_some());
        drop(state);
        let lifecycle =
            AgentLifecycle::new_with_save_guard(workspace, hard_stop.clone());
        assert!(
            lifecycle.begin_turn().is_none(),
            "hard unsaved state still admitted another turn"
        );
        assert_eq!(
            retry_delay(1_000),
            std::time::Duration::from_secs(MAX_SAVE_RETRY_SECS),
            "permanent failure backoff exceeded its cap"
        );
    }
}
