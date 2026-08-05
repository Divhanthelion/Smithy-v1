//! Session persistence.
//!
//! ## Why this exists, and why it is shaped this way
//!
//! coda banned cross-session history outright — "**NEVER** add persistent
//! cross-session history / transcripts / resume — an explicit product
//! constraint". For a throwaway terminal tool that is defensible. For an IDE it
//! is not: closing the window must not destroy the conversation that explains
//! why the code looks the way it does.
//!
//! But the constraint was protecting something real, and dropping it naively
//! would break it. The whole architecture assumes the model's cached prefix
//! stays warm, and a cache hit is a **strict prefix match on the exact bytes**.
//! Re-rendering a conversation on resume — re-formatting tool results,
//! re-ordering fields, regenerating a system prompt that embeds anything
//! variable — produces a different byte sequence for the same logical
//! conversation, and the endpoint has to prefill all of it from cold. At real
//! context sizes that is minutes of latency on the first message after a
//! restart.
//!
//! So persistence here has exactly one rule: **store the messages, replay them
//! verbatim**. No summarization, no compaction, no re-rendering, no
//! "helpfully" dropping old tool output. The round-trip is byte-identical or it
//! is a bug, and there is a test that says so.

use std::collections::HashMap;
use std::fmt::{self, Write as FmtWrite};
use std::fs::{File, OpenOptions};
use std::io::Write as IoWrite;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::limits::Limits;
use crate::message::{History, Message};
use crate::provider::Sampling;
use crate::session::SessionAccounting;

/// A stored session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredSession {
    /// Schema version, so a future format change can migrate rather than
    /// silently misread an old file.
    pub version: u32,
    pub id: String,
    /// Unix seconds. Stored as metadata *about* the session — deliberately not
    /// interpolated into the system prompt, where it would change the cache root
    /// on every run.
    pub created_at: u64,
    pub updated_at: u64,
    pub workspace: PathBuf,
    pub model: String,
    /// The connection and exact tool contract this history was built under.
    ///
    /// Sessions from format v1 have no binding and stay readable, but are never
    /// automatically replayed: a plausible transcript sent to a different
    /// account or schema is more dangerous than an explicit fresh start.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binding: Option<SessionBinding>,
    /// A short label for a session list. Derived from the first user message.
    pub title: String,
    pub sampling: Sampling,
    pub limits: Limits,
    /// Cost/context baselines kept beside History, never inside it.
    #[serde(default)]
    pub accounting: SessionAccounting,
    /// Monotonic save ordering for this session id.
    ///
    /// Format-v1 files and callers that do not coordinate revisions read as
    /// zero. App saves use positive values, which lets the store reject a late
    /// older snapshot after a newer one reached disk.
    #[serde(default)]
    pub revision: u64,
    pub messages: Vec<Message>,
    /// The model's reasoning channel, kept **beside** the messages and never in
    /// them.
    ///
    /// This is the whole trick. Reasoning must not enter [`History`] — the
    /// endpoint does not replay it, and putting it there would change the cached
    /// prefix on every turn, which is the one thing this crate is built not to
    /// do. But discarding it entirely, which is what happened before, meant the
    /// most interesting record of a long session was gone the moment the panel
    /// cleared. A sidecar keeps both properties: `into_history` still
    /// round-trips byte-exactly, and the traces survive.
    ///
    /// `#[serde(default)]` so every session written before this parses.
    #[serde(default)]
    pub reasoning: Vec<ReasoningEntry>,
    /// Terminal status for turns whose visible outcome is not fully represented
    /// by provider messages (especially stopped and failed turns).
    ///
    /// Kept beside History so recording an app/provider failure cannot alter the
    /// byte prefix replayed to the model.
    #[serde(default)]
    pub turn_outcomes: Vec<TurnOutcomeEntry>,
}

/// The dimensions that must match before stored bytes can be replayed.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionBinding {
    provider_family: String,
    endpoint_fingerprint: Fingerprint,
    configured_model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    credential_fingerprint: Option<Fingerprint>,
    tool_schema_fingerprint: Fingerprint,
    /// Canonical workspace identity. Optional only so format-v2 files remain
    /// readable; a missing value never compares equal to a current binding.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    workspace_fingerprint: Option<Fingerprint>,
}

impl fmt::Debug for SessionBinding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SessionBinding")
            .field("provider_family", &self.provider_family)
            .field("endpoint_fingerprint", &"[redacted]")
            .field("configured_model", &self.configured_model)
            .field(
                "credential_fingerprint",
                &self.credential_fingerprint.as_ref().map(|_| "[redacted]"),
            )
            .field("tool_schema_fingerprint", &"[redacted]")
            .field("workspace_fingerprint", &"[redacted]")
            .finish()
    }
}

/// A digest persisted for equality only.
///
/// Custom `Debug` is deliberate. A digest is not the secret it came from, but
/// putting raw account or schema identifiers in an error/UI string would turn
/// an internal compatibility mechanism into a tracking identifier.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
struct Fingerprint(String);

impl fmt::Debug for Fingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[redacted]")
    }
}

/// An account identity derived while the credential is still in the provider
/// builder.
///
/// The wrapper is public because the app carries it from provider construction
/// to schema construction, but its digest is deliberately inaccessible and its
/// `Debug` output is redacted. That keeps the raw credential out of app state
/// without creating a new route for fingerprints to reach UI text.
#[derive(Clone, PartialEq, Eq)]
pub struct CredentialAccountFingerprint(Fingerprint);

impl CredentialAccountFingerprint {
    pub(crate) fn from_secret(provider_family: &str, secret: &str) -> Self {
        Self(fingerprint(
            b"credential-account",
            &[provider_family.as_bytes(), secret.as_bytes()],
        ))
    }
}

impl fmt::Debug for CredentialAccountFingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[redacted]")
    }
}

/// Why a stored conversation cannot be safely replayed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BindingMismatch {
    ProviderFamily,
    Endpoint,
    ConfiguredModel,
    CredentialAccount,
    ToolSchema,
    Workspace,
}

impl BindingMismatch {
    fn label(self) -> &'static str {
        match self {
            Self::ProviderFamily => "provider family",
            Self::Endpoint => "normalized endpoint",
            Self::ConfiguredModel => "configured model",
            Self::CredentialAccount => "credential/account",
            Self::ToolSchema => "tool schema",
            Self::Workspace => "canonical workspace",
        }
    }
}

/// Compatibility result for one stored session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResumeCompatibility {
    Exact,
    /// Format v1, or another stored value with no binding.
    Unbound { version: u32 },
    Mismatch(Vec<BindingMismatch>),
}

impl ResumeCompatibility {
    /// Explain a fresh start without exposing either persisted fingerprint.
    pub fn fresh_start_notice(&self, subject: &str) -> Option<String> {
        let why = match self {
            Self::Exact => return None,
            Self::Unbound { version: 1 } => format!(
                "{subject} is format v1 and has no provider, account, or tool binding"
            ),
            Self::Unbound { .. } => {
                format!("{subject} has no provider, account, or tool binding")
            }
            Self::Mismatch(dimensions) => {
                let labels = dimensions
                    .iter()
                    .map(|dimension| dimension.label())
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("{subject} differs in: {labels}")
            }
        };
        Some(format!(
            "Started a fresh session instead of replaying conversation history: {why}."
        ))
    }
}

/// The result of searching newest-first for something safe to replay.
#[derive(Debug, Clone)]
pub struct ResumeDecision {
    pub session: Option<StoredSession>,
    /// A user-facing explanation when saved history existed but none matched.
    ///
    /// It names dimensions only. Secret material and raw fingerprints never
    /// leave the compatibility layer.
    pub notice: Option<String>,
    /// Files deliberately omitted from automatic selection.
    ///
    /// Future versions are warnings here rather than fatal to every usable
    /// session in the same store. Direct load remains strict.
    pub warnings: Vec<String>,
}

/// One completion's reasoning, with enough context to line it up afterwards.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ReasoningEntry {
    /// Which step of which turn produced it.
    pub step: usize,
    /// How many messages were in the history when it was emitted, so a reader
    /// can place it against the transcript.
    pub after_message: usize,
    /// Unix seconds.
    pub at: u64,
    pub text: String,
}

/// One completed turn's app-level terminal state.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TurnOutcomeEntry {
    /// Number of provider-visible messages when the turn ended.
    pub after_message: usize,
    /// Unix seconds.
    pub at: u64,
    pub status: PersistedTurnStatus,
    /// Stop explanation. Answers already live verbatim in History; failures use
    /// the structured field below so provider bodies and URLs cannot leak.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<PersistedFailure>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PersistedTurnStatus {
    Answered,
    Stopped,
    Failed,
}

/// The durable part of a provider failure.
///
/// Deliberately closed and structured: `ProviderError` can contain response
/// bodies, credential-bearing URLs, and transport error strings. None of those
/// arbitrary values are eligible for serialization.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PersistedFailure {
    pub category: PersistedFailureCategory,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http_status: Option<u16>,
    pub detail: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PersistedFailureCategory {
    Unreachable,
    ModelUnavailable,
    Authentication,
    RateLimited,
    Http,
    InvalidResponse,
    Provider,
}

impl PersistedFailure {
    pub fn from_provider_error(error: &crate::provider::ProviderError) -> Self {
        use crate::provider::ProviderError;
        match error {
            ProviderError::Unreachable { .. } => Self {
                category: PersistedFailureCategory::Unreachable,
                http_status: None,
                detail: "The configured provider could not be reached.".into(),
            },
            ProviderError::ModelNotLoaded { .. } => Self {
                category: PersistedFailureCategory::ModelUnavailable,
                http_status: None,
                detail: "The configured model was unavailable.".into(),
            },
            ProviderError::Http { status, .. } if matches!(status, 401 | 403) => Self {
                category: PersistedFailureCategory::Authentication,
                http_status: Some(*status),
                detail: "The provider rejected authentication.".into(),
            },
            ProviderError::Http { status: 429, .. } => Self {
                category: PersistedFailureCategory::RateLimited,
                http_status: Some(429),
                detail: "The provider rate limit was reached.".into(),
            },
            ProviderError::Http { status, .. } => Self {
                category: PersistedFailureCategory::Http,
                http_status: Some(*status),
                detail: "The provider returned an HTTP error.".into(),
            },
            ProviderError::BadResponse(_) => Self {
                category: PersistedFailureCategory::InvalidResponse,
                http_status: None,
                detail: "The provider returned an invalid response.".into(),
            },
            ProviderError::Other(_) => Self {
                category: PersistedFailureCategory::Provider,
                http_status: None,
                detail: "The provider could not complete the turn.".into(),
            },
        }
    }
}

/// Format v3 adds canonical workspace identity to the v2 provider/account/tool
/// binding. V1 remains readable but deliberately unbound; v2 remains readable
/// but cannot auto-resume because it has no workspace identity.
pub const SCHEMA_VERSION: u32 = 3;

impl SessionBinding {
    /// Bind history to the exact connection and serialized tool array.
    ///
    /// The credential is consumed only by SHA-256 and is never retained. The
    /// schema is hashed from its compact JSON bytes, not a semantic projection:
    /// field or array order is part of the provider's prefix and therefore part
    /// of compatibility.
    pub fn new(
        provider_family: &str,
        endpoint: &str,
        configured_model: &str,
        credential: Option<&str>,
        tool_schema: &serde_json::Value,
        workspace: &Path,
    ) -> Result<Self, String> {
        let credential_fingerprint = credential
            .map(|secret| CredentialAccountFingerprint::from_secret(provider_family, secret));
        Self::new_with_credential_fingerprint(
            provider_family,
            endpoint,
            configured_model,
            credential_fingerprint,
            tool_schema,
            workspace,
        )
    }

    /// Bind history when provider construction has already discarded the
    /// credential and retained only its opaque account fingerprint.
    pub fn new_with_credential_fingerprint(
        provider_family: &str,
        endpoint: &str,
        configured_model: &str,
        credential_fingerprint: Option<CredentialAccountFingerprint>,
        tool_schema: &serde_json::Value,
        workspace: &Path,
    ) -> Result<Self, String> {
        let normalized_endpoint = normalize_endpoint(endpoint)?;
        let canonical_workspace = std::fs::canonicalize(workspace).map_err(|error| {
            format!(
                "cannot identify workspace {} for session binding: {error}",
                workspace.display()
            )
        })?;
        let schema = serde_json::to_vec(tool_schema)
            .map_err(|e| format!("cannot fingerprint the tool schema: {e}"))?;
        Ok(Self {
            provider_family: provider_family.to_string(),
            endpoint_fingerprint: fingerprint(
                b"normalized-endpoint",
                &[normalized_endpoint.as_bytes()],
            ),
            configured_model: configured_model.to_string(),
            credential_fingerprint: credential_fingerprint.map(|fingerprint| fingerprint.0),
            tool_schema_fingerprint: fingerprint(b"tool-schema", &[&schema]),
            workspace_fingerprint: Some(fingerprint(
                b"canonical-workspace",
                &[canonical_workspace.as_os_str().as_encoded_bytes()],
            )),
        })
    }

    /// Compare every replay-sensitive dimension without exposing fingerprints.
    pub fn compatibility(&self, current: &SessionBinding) -> ResumeCompatibility {
        let mut mismatches = Vec::new();
        if self.provider_family != current.provider_family {
            mismatches.push(BindingMismatch::ProviderFamily);
        }
        if self.endpoint_fingerprint != current.endpoint_fingerprint {
            mismatches.push(BindingMismatch::Endpoint);
        }
        if self.configured_model != current.configured_model {
            mismatches.push(BindingMismatch::ConfiguredModel);
        }
        if self.credential_fingerprint != current.credential_fingerprint {
            mismatches.push(BindingMismatch::CredentialAccount);
        }
        if self.tool_schema_fingerprint != current.tool_schema_fingerprint {
            mismatches.push(BindingMismatch::ToolSchema);
        }
        if self.workspace_fingerprint != current.workspace_fingerprint {
            mismatches.push(BindingMismatch::Workspace);
        }
        if mismatches.is_empty() {
            ResumeCompatibility::Exact
        } else {
            ResumeCompatibility::Mismatch(mismatches)
        }
    }
}

/// Normalize spelling differences that do not select a different endpoint.
///
/// `Url` lowercases the scheme/host and removes default ports. Repeated trailing
/// slashes are removed from non-root paths because providers append the same
/// route either way. A fragment is not sent over HTTP and cannot identify a
/// provider, so it is discarded.
fn normalize_endpoint(endpoint: &str) -> Result<String, String> {
    let mut url = reqwest::Url::parse(endpoint.trim())
        .map_err(|e| format!("cannot normalize the configured endpoint: {e}"))?;
    url.set_fragment(None);
    let path = url.path().trim_end_matches('/').to_string();
    url.set_path(if path.is_empty() { "/" } else { &path });
    let mut normalized = url.to_string();
    if url.query().is_none() {
        while normalized.ends_with('/') {
            normalized.pop();
        }
    }
    Ok(normalized)
}

fn fingerprint(domain: &[u8], parts: &[&[u8]]) -> Fingerprint {
    let mut digest = Sha256::new();
    digest.update(b"smithy-session-binding-v1");
    digest.update((domain.len() as u64).to_be_bytes());
    digest.update(domain);
    for part in parts {
        digest.update((part.len() as u64).to_be_bytes());
        digest.update(part);
    }
    let bytes = digest.finalize();
    let mut hex = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        // Writing into a String cannot fail.
        let _ = write!(&mut hex, "{byte:02x}");
    }
    Fingerprint(hex)
}

impl StoredSession {
    pub fn from_history(
        id: impl Into<String>,
        workspace: &Path,
        model: &str,
        history: &History,
        sampling: &Sampling,
        limits: &Limits,
    ) -> StoredSession {
        Self::from_history_with_reasoning(id, workspace, model, history, sampling, limits, Vec::new())
    }

    /// As [`StoredSession::from_history`], keeping the reasoning sidecar.
    #[allow(clippy::too_many_arguments)]
    pub fn from_history_with_reasoning(
        id: impl Into<String>,
        workspace: &Path,
        model: &str,
        history: &History,
        sampling: &Sampling,
        limits: &Limits,
        reasoning: Vec<ReasoningEntry>,
    ) -> StoredSession {
        let now = unix_seconds();
        let messages = history.messages().to_vec();
        StoredSession {
            version: SCHEMA_VERSION,
            id: id.into(),
            created_at: now,
            updated_at: now,
            workspace: workspace.to_path_buf(),
            model: model.to_string(),
            binding: None,
            title: derive_title(&messages),
            sampling: sampling.clone(),
            limits: limits.clone(),
            accounting: SessionAccounting::default(),
            revision: 0,
            messages,
            reasoning,
            turn_outcomes: Vec::new(),
        }
    }

    /// Capture all replay and accounting state without putting sidecars in
    /// [`History`].
    #[allow(clippy::too_many_arguments)]
    pub fn from_session_state(
        id: impl Into<String>,
        workspace: &Path,
        model: &str,
        binding: SessionBinding,
        revision: u64,
        history: &History,
        sampling: &Sampling,
        limits: &Limits,
        reasoning: Vec<ReasoningEntry>,
        turn_outcomes: Vec<TurnOutcomeEntry>,
        accounting: SessionAccounting,
    ) -> StoredSession {
        let mut stored = Self::from_history_with_reasoning(
            id, workspace, model, history, sampling, limits, reasoning,
        );
        stored.binding = Some(binding);
        stored.revision = revision;
        stored.accounting = accounting;
        stored.turn_outcomes = turn_outcomes;
        stored
    }

    pub fn compatibility(&self, current: &SessionBinding) -> ResumeCompatibility {
        match &self.binding {
            Some(binding) => binding.compatibility(current),
            None => ResumeCompatibility::Unbound {
                version: self.version,
            },
        }
    }

    /// Rebuild the history exactly as it was.
    pub fn into_history(self) -> History {
        History::from_messages(self.messages)
    }
}

/// A short label from the first user message.
fn derive_title(messages: &[Message]) -> String {
    let first = messages
        .iter()
        .find(|m| m.role == crate::message::Role::User)
        .map(|m| m.content.as_str())
        .unwrap_or("(empty session)");
    let flat = first.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= 60 {
        flat
    } else {
        flat.chars().take(57).collect::<String>() + "…"
    }
}

pub(crate) fn unix_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Pick the newest exact match, not merely the newest conversation.
///
/// This is what makes switching provider/model/account and switching back
/// useful: an incompatible newest file does not hide an older compatible one.
pub fn select_resume(
    sessions: impl IntoIterator<Item = StoredSession>,
    current: &SessionBinding,
) -> ResumeDecision {
    let mut sessions: Vec<StoredSession> = sessions
        .into_iter()
        .filter(|session| session.messages.len() > 1)
        .collect();
    sessions.sort_by(|a, b| {
        b.updated_at
            .cmp(&a.updated_at)
            .then_with(|| b.id.cmp(&a.id))
    });
    if let Some(session) = sessions
        .iter()
        .find(|session| session.compatibility(current) == ResumeCompatibility::Exact)
    {
        return ResumeDecision {
            session: Some(session.clone()),
            notice: None,
            warnings: Vec::new(),
        };
    }

    let notice = sessions.first().and_then(|session| {
        session
            .compatibility(current)
            .fresh_start_notice("the newest saved conversation")
    });

    ResumeDecision {
        session: None,
        notice,
        warnings: Vec::new(),
    }
}

/// A directory of stored sessions, one JSON file each.
///
/// One file per session rather than a database: sessions are written whole and
/// read whole, never queried, and a plain file the user can read, diff, or
/// delete with `rm` is worth more here than indexed access.
#[derive(Clone)]
pub struct SessionStore {
    root: PathBuf,
}

/// Result of a compare-and-swap save.
#[derive(Debug, Clone)]
pub enum SaveOutcome {
    Saved,
    /// The same snapshot already reached disk.
    Unchanged,
    /// Disk contains a strict continuation of this stale snapshot.
    Superseded { current: StoredSession },
    /// Both branches contained content the other did not. The candidate was
    /// preserved under a fresh id instead of replacing either branch.
    Forked {
        original_id: String,
        forked: StoredSession,
        reason: String,
    },
}

/// Sessions safe to consider plus files deliberately skipped.
#[derive(Debug, Clone, Default)]
pub struct SessionListing {
    pub sessions: Vec<StoredSession>,
    pub warnings: Vec<String>,
}

impl SessionStore {
    pub fn new(root: impl Into<PathBuf>) -> Result<SessionStore, String> {
        let root = root.into();
        std::fs::create_dir_all(&root)
            .map_err(|e| format!("cannot create session store at {}: {e}", root.display()))?;
        Ok(SessionStore { root })
    }

    /// The default location: `~/.local/share/smithy/sessions`.
    pub fn default_location() -> Result<SessionStore, String> {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .ok_or("HOME is not set; pass an explicit session store path")?;
        SessionStore::new(home.join(".local/share/smithy/sessions"))
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn path_for(&self, id: &str) -> Result<PathBuf, String> {
        validate_session_id(id)?;
        Ok(self.root.join(format!("{id}.json")))
    }

    /// Write a session, preserving when it was first created.
    ///
    /// `created_at` comes from whatever is already on disk under this id, not
    /// from the value handed in. The caller builds a `StoredSession` from the
    /// live history on every save — `StoredSession::from_history` stamps *now* —
    /// so trusting the argument meant `created_at` was rewritten on every turn
    /// and a session's age was always zero. Reading it back here fixes it for
    /// every caller rather than for the one that happened to be wrong.
    pub fn save(&self, session: &StoredSession) -> Result<SaveOutcome, String> {
        let mut session = session.clone();
        validate_session_id(&session.id)?;
        session.version = SCHEMA_VERSION;
        let final_path = self.path_for(&session.id)?;
        let save_lock = save_lock_for(&self.root);
        let _guard = save_lock.lock().unwrap_or_else(|error| error.into_inner());
        let _lease = WriterLease::acquire(&self.root)?;

        session.updated_at = unix_seconds();
        match std::fs::read_to_string(&final_path) {
            Ok(text) => match decode_session(&session.id, &text) {
                Ok(existing) => {
                    session.created_at = existing.created_at;
                    if existing.binding != session.binding
                        || existing.workspace != session.workspace
                        || existing.model != session.model
                    {
                        return self.fork_conflict(
                            session,
                            "the session id was already bound to a different workspace or provider",
                        );
                    }
                    if existing.revision == session.revision {
                        if snapshot_content_identical(&existing, &session)? {
                            return Ok(SaveOutcome::Unchanged);
                        }
                        return self.fork_conflict(
                            session,
                            "equal revisions carried different persisted content",
                        );
                    }
                    let relation = snapshot_relation(&existing, &session)?;
                    match relation {
                        SnapshotRelation::CandidatePrefix => {
                            return Ok(SaveOutcome::Superseded { current: existing });
                        }
                        SnapshotRelation::ExistingPrefix
                            if session.revision > existing.revision
                                || session.revision == 0
                                || existing.revision == 0 => {}
                        SnapshotRelation::Equal if session.revision > existing.revision => {}
                        SnapshotRelation::Equal => {
                            return Ok(SaveOutcome::Superseded { current: existing });
                        }
                        SnapshotRelation::ExistingPrefix | SnapshotRelation::Diverged => {
                            return self.fork_conflict(
                                session,
                                "save history or sidecars diverged from the current on-disk branch",
                            );
                        }
                    }
                }
                Err(DecodeError::Future(message)) => return Err(message),
                // A truncated old file can be repaired by the next complete
                // snapshot. Treating corruption as immutable would preserve the
                // one copy known not to be usable.
                Err(DecodeError::Invalid(_)) => {}
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(format!(
                    "cannot inspect existing session {}: {error}",
                    session.id
                ))
            }
        }

        write_session_file(&final_path, &session)?;
        Ok(SaveOutcome::Saved)
    }

    pub fn load(&self, id: &str) -> Result<StoredSession, String> {
        let path = self.path_for(id)?;
        let text =
            std::fs::read_to_string(&path).map_err(|e| format!("cannot read session {id}: {e}"))?;
        decode_session(id, &text).map_err(DecodeError::into_message)
    }

    pub fn delete(&self, id: &str) -> Result<(), String> {
        let path = self.path_for(id)?;
        let save_lock = save_lock_for(&self.root);
        let _guard = save_lock.lock().unwrap_or_else(|error| error.into_inner());
        let _lease = WriterLease::acquire(&self.root)?;
        std::fs::remove_file(path).map_err(|e| format!("cannot delete session {id}: {e}"))?;
        File::open(&self.root)
            .and_then(|directory| directory.sync_all())
            .map_err(|e| format!("cannot flush session directory after deleting {id}: {e}"))
    }

    /// Every stored session, most recently updated first.
    pub fn list(&self) -> Result<Vec<StoredSession>, String> {
        let listing = self.list_with_warnings()?;
        for warning in &listing.warnings {
            eprintln!("[session] {warning}");
        }
        Ok(listing.sessions)
    }

    /// Every readable stored session plus safe diagnostics for skipped files.
    pub fn list_with_warnings(&self) -> Result<SessionListing, String> {
        let entries = std::fs::read_dir(&self.root)
            .map_err(|e| format!("cannot list {}: {e}", self.root.display()))?;

        let mut sessions = Vec::new();
        let mut warnings = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            if let Ok(text) = std::fs::read_to_string(&path) {
                let id = path
                    .file_stem()
                    .and_then(|name| name.to_str())
                    .unwrap_or("(unknown)");
                match decode_session(id, &text) {
                    Ok(session) => sessions.push(session),
                    Err(DecodeError::Future(message)) => warnings.push(format!(
                        "Skipped {id}.json during session selection: {message}"
                    )),
                    Err(DecodeError::Invalid(message)) => warnings.push(format!(
                        "Skipped {id}.json during session selection: {message}"
                    )),
                }
            }
        }
        sessions.sort_by(|a, b| {
            b.updated_at
                .cmp(&a.updated_at)
                .then_with(|| b.id.cmp(&a.id))
        });
        Ok(SessionListing { sessions, warnings })
    }

    /// Newest exact-compatible non-empty session, or a precise fresh-start
    /// notice when only incompatible history exists.
    pub fn select_resume(&self, binding: &SessionBinding) -> Result<ResumeDecision, String> {
        let listing = self.list_with_warnings()?;
        let mut decision = select_resume(listing.sessions, binding);
        decision.warnings = listing.warnings;
        Ok(decision)
    }

    fn fork_conflict(
        &self,
        mut candidate: StoredSession,
        reason: &str,
    ) -> Result<SaveOutcome, String> {
        let original_id = candidate.id.clone();
        let forked_id = self.unique_conflict_id(&original_id)?;
        candidate.id = forked_id;
        candidate.created_at = unix_seconds();
        candidate.updated_at = candidate.created_at;
        let path = self.path_for(&candidate.id)?;
        write_session_file(&path, &candidate)?;
        Ok(SaveOutcome::Forked {
            original_id,
            forked: candidate,
            reason: reason.to_string(),
        })
    }

    fn unique_conflict_id(&self, original_id: &str) -> Result<String, String> {
        for _ in 0..1024 {
            let id = conflict_session_id(original_id)?;
            if !self.path_for(&id)?.exists() {
                return Ok(id);
            }
        }
        Err(format!(
            "cannot allocate a conflict id for session {original_id}"
        ))
    }
}

#[derive(Debug)]
enum DecodeError {
    Future(String),
    Invalid(String),
}

/// APFS commonly allows 255 bytes per component. Session files add `.json`, and
/// temp files add more, so accepting an unbounded id made a second conflict fork
/// fail with `ENAMETOOLONG` even though the first save succeeded.
const MAX_SESSION_ID_BYTES: usize = 180;
/// Enough of the original id to remain recognizable without letting repeated
/// `-conflict-...` suffixes grow forever.
const CONFLICT_ASSOCIATION_BYTES: usize = 96;
/// Ninety-six digest bits plus pid/time/counter uniqueness keeps conflict names
/// collision-resistant without spending the filesystem's whole name budget.
const CONFLICT_DIGEST_HEX: usize = 24;

/// Allocate a bounded lineage id before a divergent in-memory save can reach
/// disk. The store still checks existence under its writer lease.
pub fn conflict_session_id(original_id: &str) -> Result<String, String> {
    static CONFLICT_COUNTER: AtomicU64 = AtomicU64::new(0);
    validate_session_id(original_id)?;
    let root = original_id
        .split_once("-conflict-")
        .map(|(root, _)| root)
        .unwrap_or(original_id);
    let root = &root[..root.len().min(CONFLICT_ASSOCIATION_BYTES)];
    let association = fingerprint(b"conflict-association", &[original_id.as_bytes()]);
    let id = format!(
        "{root}-conflict-{}-{:x}-{:x}-{:x}",
        &association.0[..CONFLICT_DIGEST_HEX],
        unix_seconds(),
        std::process::id(),
        CONFLICT_COUNTER.fetch_add(1, Ordering::Relaxed)
    );
    validate_session_id(&id)?;
    Ok(id)
}

fn validate_session_id(id: &str) -> Result<(), String> {
    let mut components = Path::new(id).components();
    let one_normal = matches!(components.next(), Some(Component::Normal(component)) if component == id);
    if id.is_empty()
        || id.len() > MAX_SESSION_ID_BYTES
        || !one_normal
        || components.next().is_some()
        || id == "."
        || id == ".."
        || id.chars().any(|character| {
            character.is_control()
                || !(character.is_ascii_alphanumeric()
                    || matches!(character, '-' | '_' | '.'))
        })
    {
        return Err(format!(
            "session id {id:?} is not one bounded safe filename component"
        ));
    }
    Ok(())
}

struct WriterLease(File);

impl WriterLease {
    fn acquire(root: &Path) -> Result<Self, String> {
        let path = root.join(".writer.lock");
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|error| format!("cannot open session writer lease: {error}"))?;
        file.lock_exclusive()
            .map_err(|error| format!("cannot acquire session writer lease: {error}"))?;
        Ok(Self(file))
    }
}

impl Drop for WriterLease {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.0);
    }
}

/// Semantic ancestry across provider History and every append-only sidecar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapshotRelation {
    Equal,
    ExistingPrefix,
    CandidatePrefix,
    Diverged,
}

/// Compare snapshots without using revisions as a proxy for ancestry.
pub fn snapshot_relation(
    existing: &StoredSession,
    candidate: &StoredSession,
) -> Result<SnapshotRelation, String> {
    if existing.binding != candidate.binding
        || existing.workspace != candidate.workspace
        || existing.model != candidate.model
    {
        return Ok(SnapshotRelation::Diverged);
    }
    Ok(combine_relations(
        history_relation(&existing.messages, &candidate.messages)?,
        sidecar_relation(existing, candidate),
    ))
}

fn history_relation(
    existing: &[Message],
    candidate: &[Message],
) -> Result<SnapshotRelation, String> {
    let existing = existing
        .iter()
        .map(serde_json::to_vec)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("cannot compare existing session history: {error}"))?;
    let candidate = candidate
        .iter()
        .map(serde_json::to_vec)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("cannot compare candidate session history: {error}"))?;
    let common = existing.len().min(candidate.len());
    if existing[..common] != candidate[..common] {
        return Ok(SnapshotRelation::Diverged);
    }
    Ok(match existing.len().cmp(&candidate.len()) {
        std::cmp::Ordering::Equal => SnapshotRelation::Equal,
        std::cmp::Ordering::Less => SnapshotRelation::ExistingPrefix,
        std::cmp::Ordering::Greater => SnapshotRelation::CandidatePrefix,
    })
}

fn sidecar_relation(existing: &StoredSession, candidate: &StoredSession) -> SnapshotRelation {
    [
        slice_relation(&existing.reasoning, &candidate.reasoning),
        slice_relation(&existing.turn_outcomes, &candidate.turn_outcomes),
        accounting_relation(&existing.accounting, &candidate.accounting),
    ]
    .into_iter()
    .fold(SnapshotRelation::Equal, combine_relations)
}

fn slice_relation<T: PartialEq>(existing: &[T], candidate: &[T]) -> SnapshotRelation {
    let common = existing.len().min(candidate.len());
    if existing[..common] != candidate[..common] {
        return SnapshotRelation::Diverged;
    }
    match existing.len().cmp(&candidate.len()) {
        std::cmp::Ordering::Equal => SnapshotRelation::Equal,
        std::cmp::Ordering::Less => SnapshotRelation::ExistingPrefix,
        std::cmp::Ordering::Greater => SnapshotRelation::CandidatePrefix,
    }
}

fn accounting_relation(
    existing: &SessionAccounting,
    candidate: &SessionAccounting,
) -> SnapshotRelation {
    if existing.system_base_chars != candidate.system_base_chars
        || existing.project_context_chars != candidate.project_context_chars
    {
        return SnapshotRelation::Diverged;
    }
    let usage = monotonic_usage_relation(&existing.usage, &candidate.usage);
    let calibration = match (
        existing.ledger_calibration,
        candidate.ledger_calibration,
    ) {
        (None, None) => SnapshotRelation::Equal,
        (Some(left), Some(right)) if left == right => SnapshotRelation::Equal,
        (None, Some(_)) => SnapshotRelation::ExistingPrefix,
        (Some(_), None) => SnapshotRelation::CandidatePrefix,
        (Some(_), Some(_)) => SnapshotRelation::Diverged,
    };
    let relation = combine_relations(usage, calibration);
    if relation == SnapshotRelation::Equal
        && (existing.last_prompt_tokens != candidate.last_prompt_tokens
            || existing.last_cached_tokens != candidate.last_cached_tokens)
    {
        SnapshotRelation::Diverged
    } else {
        relation
    }
}

fn monotonic_usage_relation(
    existing: &crate::session::Usage,
    candidate: &crate::session::Usage,
) -> SnapshotRelation {
    let existing_values = [
        existing.prompt_tokens,
        existing.completion_tokens,
        existing.cached_tokens,
        existing.reasoning_tokens,
        i64::try_from(existing.requests).unwrap_or(i64::MAX),
    ];
    let candidate_values = [
        candidate.prompt_tokens,
        candidate.completion_tokens,
        candidate.cached_tokens,
        candidate.reasoning_tokens,
        i64::try_from(candidate.requests).unwrap_or(i64::MAX),
    ];
    let existing_le_candidate = existing_values
        .iter()
        .zip(&candidate_values)
        .all(|(existing, candidate)| existing <= candidate);
    let candidate_le_existing = existing_values
        .iter()
        .zip(&candidate_values)
        .all(|(existing, candidate)| candidate <= existing);
    match (existing_le_candidate, candidate_le_existing) {
        (true, true) => SnapshotRelation::Equal,
        (true, false) => SnapshotRelation::ExistingPrefix,
        (false, true) => SnapshotRelation::CandidatePrefix,
        (false, false) => SnapshotRelation::Diverged,
    }
}

fn combine_relations(left: SnapshotRelation, right: SnapshotRelation) -> SnapshotRelation {
    use SnapshotRelation::{CandidatePrefix, Diverged, Equal, ExistingPrefix};
    match (left, right) {
        (Diverged, _) | (_, Diverged) => Diverged,
        (Equal, relation) | (relation, Equal) => relation,
        (ExistingPrefix, ExistingPrefix) => ExistingPrefix,
        (CandidatePrefix, CandidatePrefix) => CandidatePrefix,
        (ExistingPrefix, CandidatePrefix) | (CandidatePrefix, ExistingPrefix) => Diverged,
    }
}

fn snapshot_content_identical(
    existing: &StoredSession,
    candidate: &StoredSession,
) -> Result<bool, String> {
    let mut existing = existing.clone();
    let mut candidate = candidate.clone();
    existing.created_at = 0;
    existing.updated_at = 0;
    candidate.created_at = 0;
    candidate.updated_at = 0;
    let existing = serde_json::to_vec(&existing)
        .map_err(|error| format!("cannot compare existing session snapshot: {error}"))?;
    let candidate = serde_json::to_vec(&candidate)
        .map_err(|error| format!("cannot compare candidate session snapshot: {error}"))?;
    Ok(existing == candidate)
}

fn write_session_file(final_path: &Path, session: &StoredSession) -> Result<(), String> {
    let json = serde_json::to_string_pretty(session)
        .map_err(|error| format!("cannot serialize session {}: {error}", session.id))?;
    write_atomic(final_path, json.as_bytes())
}

impl DecodeError {
    fn into_message(self) -> String {
        match self {
            Self::Future(message) | Self::Invalid(message) => message,
        }
    }
}

/// The single schema gate used by load, list, and the transcript exporter.
pub fn decode_session_json(id: &str, text: &str) -> Result<StoredSession, String> {
    decode_session(id, text).map_err(DecodeError::into_message)
}

fn decode_session(id: &str, text: &str) -> Result<StoredSession, DecodeError> {
    validate_session_id(id).map_err(DecodeError::Invalid)?;
    let value: serde_json::Value = serde_json::from_str(text).map_err(|error| {
        DecodeError::Invalid(format!(
            "session {id} is not readable ({error}); it may be damaged"
        ))
    })?;
    let version = value
        .get("version")
        .and_then(serde_json::Value::as_u64)
        .and_then(|version| u32::try_from(version).ok())
        .ok_or_else(|| {
            DecodeError::Invalid(format!("session {id} has no readable format version"))
        })?;
    if version > SCHEMA_VERSION {
        return Err(DecodeError::Future(format!(
            "session {id} was written by a newer version of Smithy (format {version} > \
             {SCHEMA_VERSION})"
        )));
    }
    if version == 0 {
        return Err(DecodeError::Invalid(format!(
            "session {id} uses unsupported format 0"
        )));
    }

    let mut session: StoredSession = serde_json::from_value(value).map_err(|error| {
        DecodeError::Invalid(format!(
            "session {id} is not readable ({error}); it may be damaged"
        ))
    })?;
    validate_session_id(&session.id).map_err(DecodeError::Invalid)?;
    if session.id != id {
        return Err(DecodeError::Invalid(format!(
            "session id {:?} does not match filename stem {id:?}",
            session.id
        )));
    }
    if version == 1 {
        // V1 had none of these fields. Force the migration even if a hand-edited
        // file smuggles similarly named values in under the old version: v1 is
        // unbound by definition and must never become auto-replayable.
        session.binding = None;
        session.accounting = SessionAccounting::default();
        session.revision = 0;
        session.turn_outcomes.clear();
    }
    Ok(session)
}

fn save_lock_for(path: &Path) -> Arc<Mutex<()>> {
    static LOCKS: OnceLock<Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>> = OnceLock::new();
    let locks = LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut locks = locks.lock().unwrap_or_else(|error| error.into_inner());
    locks
        .entry(path.to_path_buf())
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone()
}

/// A same-directory name no other in-flight save can share.
///
/// Same-directory is what keeps rename atomic; pid plus a process-wide counter
/// is what prevents two session saves from clobbering one shared `.json.tmp`.
fn unique_temp_path(final_path: &Path) -> Result<PathBuf, String> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let parent = final_path
        .parent()
        .ok_or_else(|| format!("session path {} has no parent", final_path.display()))?;
    let name = final_path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| format!("session path {} has no file name", final_path.display()))?;
    Ok(parent.join(format!(
        ".{name}.{}-{}.tmp",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )))
}

fn write_atomic(final_path: &Path, bytes: &[u8]) -> Result<(), String> {
    let tmp_path = unique_temp_path(final_path)?;
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp_path)
            .map_err(|e| format!("cannot create {}: {e}", tmp_path.display()))?;
        file.write_all(bytes)
            .map_err(|e| format!("cannot write {}: {e}", tmp_path.display()))?;
        file.sync_all()
            .map_err(|e| format!("cannot flush {}: {e}", tmp_path.display()))?;
        drop(file);
        std::fs::rename(&tmp_path, final_path)
            .map_err(|e| format!("cannot finalize {}: {e}", final_path.display()))?;
        // Rename durability is a directory property. Syncing only the temp file
        // left a crash window where its bytes were durable but its final name
        // was not, so the next launch could lose an acknowledged turn.
        let parent = final_path
            .parent()
            .ok_or_else(|| format!("session path {} has no parent", final_path.display()))?;
        File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(|e| format!("cannot flush session directory: {e}"))
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp_path);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use smithy_tools::{ToolCall, ToolResult};

    fn sample_history() -> History {
        let mut h = History::new("you are smithy");
        h.push(Message::user("read notes.txt"));
        let call = ToolCall::new("c1", "read", r#"{"path":"notes.txt"}"#);
        h.push(Message::assistant_with_calls("", vec![call.clone()]));
        h.push(Message::tool_result(&ToolResult::ok(
            &call,
            "     1\tFJORD",
        )));
        h.push(Message::assistant("The file says FJORD."));
        h
    }

    fn store() -> (tempfile::TempDir, SessionStore) {
        let tmp = tempfile::tempdir().unwrap();
        let store = SessionStore::new(tmp.path()).unwrap();
        (tmp, store)
    }

    fn binding(
        provider: &str,
        endpoint: &str,
        model: &str,
        credential: Option<&str>,
        schema_marker: &str,
    ) -> SessionBinding {
        SessionBinding::new(
            provider,
            endpoint,
            model,
            credential,
            &serde_json::json!([
                {
                    "type": "function",
                    "function": { "name": schema_marker, "parameters": {} }
                }
            ]),
            std::env::temp_dir().as_path(),
        )
        .unwrap()
    }

    fn bound(id: &str, binding: SessionBinding, revision: u64) -> StoredSession {
        let mut stored = StoredSession::from_history(
            id,
            Path::new("/tmp/ws"),
            "configured-model",
            &sample_history(),
            &Sampling::default(),
            &Limits::default(),
        );
        stored.binding = Some(binding);
        stored.revision = revision;
        stored
    }

    fn mismatch_between(
        saved: SessionBinding,
        current: SessionBinding,
    ) -> Vec<BindingMismatch> {
        match saved.compatibility(&current) {
            ResumeCompatibility::Mismatch(dimensions) => dimensions,
            other => panic!("expected a mismatch, got {other:?}"),
        }
    }

    /// Format v1 has real conversations in the wild. Adding binding fields must
    /// not make them disappear from list/export or move one byte of History, but
    /// the absent identity must make automatic replay impossible.
    #[test]
    fn v1_sessions_migrate_as_unbound_without_changing_history_bytes() {
        let (_tmp, store) = store();
        let current = binding(
            "openrouter",
            "https://openrouter.ai/api/v1",
            "model-a",
            Some("account-key"),
            "read",
        );
        let stored = bound("legacy", current.clone(), 7);
        let before = serde_json::to_string(&stored.messages).unwrap();
        let mut value = serde_json::to_value(&stored).unwrap();
        let object = value.as_object_mut().unwrap();
        object.insert("version".into(), serde_json::json!(1));
        object.remove("binding");
        object.remove("accounting");
        object.remove("revision");
        object.remove("turn_outcomes");
        let raw = serde_json::to_string_pretty(&value).unwrap();
        std::fs::write(store.root().join("legacy.json"), &raw).unwrap();

        let loaded = store.load("legacy").unwrap();
        let listed = store.list().unwrap();
        let exported = decode_session_json("legacy", &raw).unwrap();

        assert_eq!(listed.len(), 1, "v1 must remain visible in session lists");
        assert_eq!(
            serde_json::to_string(&loaded.messages).unwrap(),
            before,
            "migration changed provider-visible history"
        );
        assert_eq!(
            serde_json::to_string(&exported.messages).unwrap(),
            before,
            "the exporter and store must use the same migration"
        );
        assert_eq!(
            loaded.compatibility(&current),
            ResumeCompatibility::Unbound { version: 1 }
        );
        let decision = select_resume(listed, &current);
        assert!(decision.session.is_none());
        assert!(
            decision.notice.unwrap().contains("format v1"),
            "the migration must explain why legacy history was not replayed"
        );
    }

    /// Format v2 is still replayable data in existing installs. New untrusted
    /// labels apply only to future messages; decoding must not retrofit them
    /// into a provider-visible prefix that was already sent and cached.
    #[test]
    fn v2_sessions_keep_every_history_byte_when_untrusted_labels_are_added() {
        let identity = binding("local", "https://example.test/v1", "m", None, "read");
        let stored = bound("format-two", identity, 3);
        let before = serde_json::to_string(&stored.messages).unwrap();
        let mut value = serde_json::to_value(stored).unwrap();
        let object = value.as_object_mut().unwrap();
        object.insert("version".into(), serde_json::json!(2));
        object
            .get_mut("binding")
            .and_then(serde_json::Value::as_object_mut)
            .unwrap()
            .remove("workspace_fingerprint");
        let decoded =
            decode_session_json("format-two", &serde_json::to_string(&value).unwrap()).unwrap();
        assert_eq!(serde_json::to_string(&decoded.messages).unwrap(), before);
        assert_eq!(decoded.messages[3].content, "     1\tFJORD");
        assert!(!decoded.messages[3].content.contains("UNTRUSTED_DATA"));
    }

    /// A provider family changes request semantics even when a URL/model string
    /// happens to match, so it must independently block replay.
    #[test]
    fn changing_provider_family_is_a_resume_mismatch() {
        let saved = binding("openrouter", "https://example.test/v1", "m", None, "read");
        let current = binding("deepseek", "https://example.test/v1", "m", None, "read");
        assert_eq!(
            mismatch_between(saved, current),
            vec![BindingMismatch::ProviderFamily]
        );
    }

    /// Endpoint spelling is normalized, but a genuinely different route can be
    /// a different tenant/server and must independently block replay.
    #[test]
    fn changing_normalized_endpoint_is_a_resume_mismatch() {
        let saved = binding("local", "https://example.test/v1", "m", None, "read");
        let current = binding("local", "https://example.test/v2", "m", None, "read");
        assert_eq!(
            mismatch_between(saved, current),
            vec![BindingMismatch::Endpoint]
        );
    }

    /// Model labels from probing can be decorative, but the configured model id
    /// selects the actual weights and must independently block replay.
    #[test]
    fn changing_configured_model_is_a_resume_mismatch() {
        let saved = binding("local", "https://example.test/v1", "m-a", None, "read");
        let current = binding("local", "https://example.test/v1", "m-b", None, "read");
        assert_eq!(
            mismatch_between(saved, current),
            vec![BindingMismatch::ConfiguredModel]
        );
    }

    /// Two keys at one hosted endpoint may be different accounts. Replaying one
    /// account's transcript into the other must be blocked without storing keys.
    #[test]
    fn changing_credential_account_is_a_resume_mismatch() {
        let saved = binding(
            "openrouter",
            "https://example.test/v1",
            "m",
            Some("key-a"),
            "read",
        );
        let current = binding(
            "openrouter",
            "https://example.test/v1",
            "m",
            Some("key-b"),
            "read",
        );
        assert_eq!(
            mismatch_between(saved, current),
            vec![BindingMismatch::CredentialAccount]
        );
    }

    /// Tool names alone are insufficient: changing any advertised schema byte
    /// changes both model capabilities and prefix bytes, so it blocks replay.
    #[test]
    fn changing_exact_tool_schema_is_a_resume_mismatch() {
        let saved = binding("local", "https://example.test/v1", "m", None, "read");
        let current = binding("local", "https://example.test/v1", "m", None, "write");
        assert_eq!(
            mismatch_between(saved, current),
            vec![BindingMismatch::ToolSchema]
        );
    }

    /// URL case/default-port/trailing-slash differences used to fork sessions
    /// even though every spelling reaches the same route.
    #[test]
    fn equivalent_endpoint_spellings_are_an_exact_resume_match() {
        let saved = binding(
            "local",
            " HTTPS://EXAMPLE.TEST:443/v1/// ",
            "m",
            None,
            "read",
        );
        let current = binding("local", "https://example.test/v1", "m", None, "read");
        assert_eq!(saved.compatibility(&current), ResumeCompatibility::Exact);
    }

    /// The binding needs a stable account identity, but a session file, Debug
    /// string, or user notice containing the API key would turn safety metadata
    /// into a credential leak.
    #[test]
    fn secret_material_is_never_serialized_or_exposed_by_compatibility() {
        let secret = "sk-live-never-write-this";
        let endpoint_secret = "endpoint-password-never-write-this";
        let saved = binding(
            "openrouter",
            &format!("https://user:{endpoint_secret}@example.test/v1?token={endpoint_secret}"),
            "m",
            Some(secret),
            "read",
        );
        let changed = binding(
            "openrouter",
            &format!("https://user:{endpoint_secret}@example.test/v1?token={endpoint_secret}"),
            "m",
            Some("another-account"),
            "read",
        );
        let json = serde_json::to_string(&bound("secret-test", saved.clone(), 1)).unwrap();
        let debug = format!("{saved:?}");
        let notice = saved
            .compatibility(&changed)
            .fresh_start_notice("the saved conversation")
            .unwrap();

        assert!(!json.contains(secret), "the credential reached session JSON");
        assert!(
            !json.contains(endpoint_secret),
            "secret material embedded in the endpoint reached session JSON"
        );
        assert!(!debug.contains(secret), "the credential reached Debug output");
        assert!(debug.contains("[redacted]"));
        assert!(!notice.contains(secret));
        assert!(!notice.contains("sk-"));
    }

    /// The newest conversation may belong to the provider just switched away
    /// from. Switching back must find the newest exact match rather than always
    /// forking from the globally newest file.
    #[test]
    fn exact_resume_skips_newer_incompatible_sessions_and_selects_the_match() {
        let wanted = binding("local", "https://local.test/v1", "m", None, "read");
        let other = binding(
            "openrouter",
            "https://cloud.test/v1",
            "m",
            Some("cloud-key"),
            "read",
        );
        let mut newest_incompatible = bound("newer-other-provider", other, 3);
        newest_incompatible.updated_at = 300;
        let mut newest_exact = bound("newest-exact", wanted.clone(), 4);
        newest_exact.updated_at = 200;
        let mut older_exact = bound("older-exact", wanted.clone(), 2);
        older_exact.updated_at = 100;
        let decision = select_resume(
            // Deliberately not newest-first: callers other than SessionStore
            // must get the same deterministic selection.
            vec![older_exact, newest_incompatible, newest_exact],
            &wanted,
        );

        assert_eq!(decision.session.unwrap().id, "newest-exact");
        assert!(decision.notice.is_none());
    }

    /// Silently starting over makes a lost resume look like lost data, while
    /// exposing digest strings makes an internal identifier user-facing. The
    /// Notice must name the mismatching dimension and nothing opaque.
    #[test]
    fn a_mismatch_starts_fresh_with_a_precise_dimension_notice() {
        let saved = binding("local", "https://local.test/v1", "model-a", None, "read");
        let current = binding("local", "https://local.test/v1", "model-b", None, "read");
        let decision = select_resume(vec![bound("saved", saved, 1)], &current);
        let notice = decision.notice.expect("fresh-start notice");

        assert!(decision.session.is_none());
        assert!(notice.contains("configured model"), "{notice}");
        assert!(!notice.contains("fingerprint"), "{notice}");
        assert!(
            !notice
                .split(|character: char| !character.is_ascii_hexdigit())
                .any(|word| word.len() == 64),
            "a raw SHA-256 digest reached the Notice: {notice}"
        );
    }

    /// The property the whole design turns on. If a resumed conversation
    /// serializes to different bytes than the original, the endpoint has to
    /// re-prefill it from cold.
    #[test]
    fn a_round_trip_is_byte_identical() {
        let (_t, store) = store();
        let history = sample_history();
        let before = serde_json::to_string(&history.to_api()).unwrap();

        let stored = StoredSession::from_history(
            "s1",
            Path::new("/tmp/ws"),
            "test-model",
            &history,
            &Sampling::default(),
            &Limits::default(),
        );
        store.save(&stored).unwrap();

        let after =
            serde_json::to_string(&store.load("s1").unwrap().into_history().to_api()).unwrap();
        assert_eq!(
            before, after,
            "resume must reproduce the exact prefix bytes"
        );
    }

    #[test]
    fn the_system_prompt_survives_a_round_trip() {
        let (_t, store) = store();
        let stored = StoredSession::from_history(
            "s1",
            Path::new("/tmp/ws"),
            "m",
            &sample_history(),
            &Sampling::default(),
            &Limits::default(),
        );
        store.save(&stored).unwrap();
        let restored = store.load("s1").unwrap().into_history();
        assert_eq!(restored.system_prompt(), Some("you are smithy"));
    }

    #[test]
    fn tool_call_correlation_survives_a_round_trip() {
        let (_t, store) = store();
        let stored = StoredSession::from_history(
            "s1",
            Path::new("/tmp/ws"),
            "m",
            &sample_history(),
            &Sampling::default(),
            &Limits::default(),
        );
        store.save(&stored).unwrap();
        let restored = store.load("s1").unwrap().into_history();
        let tool_msg = restored
            .messages()
            .iter()
            .find(|m| m.role == crate::message::Role::Tool)
            .unwrap();
        assert_eq!(tool_msg.tool_call_id.as_deref(), Some("c1"));
    }

    #[test]
    fn a_title_is_derived_from_the_first_user_message() {
        let stored = StoredSession::from_history(
            "s1",
            Path::new("/tmp/ws"),
            "m",
            &sample_history(),
            &Sampling::default(),
            &Limits::default(),
        );
        assert_eq!(stored.title, "read notes.txt");
    }

    #[test]
    fn a_long_title_is_shortened() {
        let mut h = History::new("sys");
        h.push(Message::user("word ".repeat(50)));
        let stored = StoredSession::from_history(
            "s",
            Path::new("/tmp"),
            "m",
            &h,
            &Sampling::default(),
            &Limits::default(),
        );
        assert!(stored.title.chars().count() <= 60);
        assert!(stored.title.ends_with('…'));
    }

    #[test]
    fn listing_is_newest_first() {
        let (_t, store) = store();
        for id in ["a", "b", "c"] {
            let mut h = History::new("sys");
            h.push(Message::user(format!("task {id}")));
            let mut s = StoredSession::from_history(
                id,
                Path::new("/tmp"),
                "m",
                &h,
                &Sampling::default(),
                &Limits::default(),
            );
            // Force a deterministic ordering rather than racing the clock.
            s.updated_at = match id {
                "a" => 100,
                "b" => 300,
                _ => 200,
            };
            let json = serde_json::to_string_pretty(&s).unwrap();
            std::fs::write(store.root().join(format!("{id}.json")), json).unwrap();
        }
        let ids: Vec<String> = store.list().unwrap().into_iter().map(|s| s.id).collect();
        assert_eq!(ids, vec!["b", "c", "a"]);
    }

    #[test]
    fn a_corrupt_file_does_not_hide_the_others() {
        let (_t, store) = store();
        let stored = StoredSession::from_history(
            "good",
            Path::new("/tmp"),
            "m",
            &sample_history(),
            &Sampling::default(),
            &Limits::default(),
        );
        store.save(&stored).unwrap();
        std::fs::write(store.root().join("bad.json"), "{ not json").unwrap();

        let listed = store.list().unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, "good");
    }

    /// `load` used to reject a future format while `list` deserialized and
    /// surfaced it. Both entry points must share one version gate or automatic
    /// selection can accept a file direct loading says is unsafe.
    #[test]
    fn selection_warns_and_skips_future_versions_while_direct_load_stays_strict() {
        let (_t, store) = store();
        let mut stored = StoredSession::from_history(
            "future",
            Path::new("/tmp"),
            "m",
            &sample_history(),
            &Sampling::default(),
            &Limits::default(),
        );
        stored.version = SCHEMA_VERSION + 1;
        std::fs::write(
            store.root().join("future.json"),
            serde_json::to_string(&stored).unwrap(),
        )
        .unwrap();
        assert!(store.load("future").unwrap_err().contains("newer version"));
        let listing = store.list_with_warnings().unwrap();
        assert!(listing.sessions.is_empty());
        assert!(listing
            .warnings
            .iter()
            .any(|warning| warning.contains("newer version")));
    }

    #[test]
    fn saving_twice_leaves_no_temp_file_behind() {
        let (_t, store) = store();
        let stored = StoredSession::from_history(
            "s1",
            Path::new("/tmp"),
            "m",
            &sample_history(),
            &Sampling::default(),
            &Limits::default(),
        );
        store.save(&stored).unwrap();
        store.save(&stored).unwrap();
        let files: Vec<String> = std::fs::read_dir(store.root())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        assert!(files.iter().any(|file| file == "s1.json"));
        assert!(!files.iter().any(|file| file.ends_with(".tmp")));
    }

    /// Save tasks can finish out of order after a slow snapshot or filesystem
    /// stall. Revision two reaching disk first must make a delayed revision one
    /// a no-op, or the next launch silently loses the newest turn.
    #[test]
    fn an_older_save_cannot_overwrite_newer_history() {
        let (_t, store) = store();
        let identity = binding("local", "https://example.test/v1", "m", None, "read");
        let mut older = bound("ordered", identity.clone(), 1);
        older.messages.push(Message::user("older turn"));
        let mut newer = older.clone();
        newer.revision = 2;
        newer.messages.push(Message::assistant("newest answer"));

        store.save(&newer).unwrap();
        store.save(&older).unwrap();

        let loaded = store.load("ordered").unwrap();
        assert_eq!(loaded.revision, 2);
        assert!(
            loaded
                .messages
                .iter()
                .any(|message| message.content == "newest answer"),
            "the late older snapshot replaced revision two"
        );
    }

    /// A fixed `.json.tmp` name lets two async saves open the same scratch file:
    /// one rename then makes the other fail or rename somebody else's bytes.
    /// Every attempt needs a unique name in the final file's directory.
    #[test]
    fn concurrent_saves_receive_unique_same_directory_temp_paths() {
        let (_t, store) = store();
        let final_path = store.path_for("same-session").unwrap();
        let first = unique_temp_path(&final_path).unwrap();
        let second = unique_temp_path(&final_path).unwrap();

        assert_ne!(first, second);
        assert_eq!(first.parent(), final_path.parent());
        assert_eq!(second.parent(), final_path.parent());
        assert_eq!(first.extension().and_then(|ext| ext.to_str()), Some("tmp"));
        assert_eq!(second.extension().and_then(|ext| ext.to_str()), Some("tmp"));
    }

    /// **A session's age must not reset every time it is saved.**
    ///
    /// The app rebuilds a `StoredSession` from the live history after every
    /// turn, and `from_history` stamps `created_at` with *now* — so a session
    /// saved on each turn was permanently zero seconds old, and `created_at`
    /// and `updated_at` were always the same number.
    #[test]
    fn re_saving_a_session_keeps_the_time_it_was_created() {
        let (_t, store) = store();
        let mut first = StoredSession::from_history(
            "s1",
            Path::new("/tmp"),
            "m",
            &sample_history(),
            &Sampling::default(),
            &Limits::default(),
        );
        first.created_at = 1_000;
        store.save(&first).unwrap();

        // A later turn builds a fresh one, stamped now, as the app does.
        let mut later = first.clone();
        later.created_at = 9_999;
        store.save(&later).unwrap();

        let loaded = store.load("s1").unwrap();
        assert_eq!(
            loaded.created_at, 1_000,
            "the session was created once; saving it again is not creating it"
        );
        assert!(
            loaded.updated_at >= loaded.created_at,
            "but it was certainly updated"
        );
    }

    /// A brand-new session keeps the timestamp it was built with — there is
    /// nothing on disk to inherit from.
    #[test]
    fn a_first_save_keeps_its_own_creation_time() {
        let (_t, store) = store();
        let mut fresh = StoredSession::from_history(
            "new",
            Path::new("/tmp"),
            "m",
            &sample_history(),
            &Sampling::default(),
            &Limits::default(),
        );
        fresh.created_at = 4_242;
        store.save(&fresh).unwrap();
        assert_eq!(store.load("new").unwrap().created_at, 4_242);
    }

    /// Which model produced a conversation is worth knowing when reading one
    /// back, and it round-trips.
    #[test]
    fn the_model_that_produced_a_session_is_recorded() {
        let (_t, store) = store();
        let stored = StoredSession::from_history(
            "s1",
            Path::new("/tmp"),
            "qwen3.6-27b · MLX 4bit",
            &sample_history(),
            &Sampling::default(),
            &Limits::default(),
        );
        store.save(&stored).unwrap();
        assert_eq!(store.load("s1").unwrap().model, "qwen3.6-27b · MLX 4bit");
    }

    #[test]
    fn delete_removes_the_session() {
        let (_t, store) = store();
        let stored = StoredSession::from_history(
            "s1",
            Path::new("/tmp"),
            "m",
            &sample_history(),
            &Sampling::default(),
            &Limits::default(),
        );
        store.save(&stored).unwrap();
        store.delete("s1").unwrap();
        assert!(store.list().unwrap().is_empty());
    }

    /// Revision allocation describes attempt order, not content ancestry. A
    /// reconnect could allocate revision three from an older snapshot after
    /// revision two already contained an answer; the larger number must not
    /// erase that answer.
    #[test]
    fn a_higher_revision_with_older_history_cannot_replace_newer_content() {
        let (_t, store) = store();
        let identity = binding("local", "https://example.test/v1", "m", None, "read");
        let mut older = bound("content-cas", identity, 1);
        older.messages.push(Message::user("question"));
        let mut newer = older.clone();
        newer.revision = 2;
        newer.messages.push(Message::assistant("answer that must survive"));
        store.save(&newer).unwrap();

        older.revision = 3;
        assert!(matches!(
            store.save(&older).unwrap(),
            SaveOutcome::Superseded { .. }
        ));
        let loaded = store.load("content-cas").unwrap();
        assert!(loaded
            .messages
            .iter()
            .any(|message| message.content == "answer that must survive"));
    }

    /// Two processes can both believe they own revision seven. Equality is safe
    /// only when every persisted byte of content agrees; otherwise both branches
    /// must remain readable under distinct safe ids.
    #[test]
    fn equal_revision_conflicts_preserve_both_branches_under_safe_ids() {
        let (_t, store) = store();
        let identity = binding("local", "https://example.test/v1", "m", None, "read");
        let mut first = bound("shared", identity, 7);
        first.messages.push(Message::assistant("window one"));
        store.save(&first).unwrap();
        assert!(matches!(
            store.save(&first).unwrap(),
            SaveOutcome::Unchanged
        ));

        let mut second = first.clone();
        second.messages.pop();
        second.messages.push(Message::assistant("window two"));
        let SaveOutcome::Forked { forked, .. } = store.save(&second).unwrap() else {
            panic!("different equal revisions must fork");
        };
        validate_session_id(&forked.id).unwrap();
        assert_ne!(forked.id, "shared");
        assert_eq!(
            store.load("shared").unwrap().messages.last().unwrap().content,
            "window one"
        );
        assert_eq!(
            store
                .load(&forked.id)
                .unwrap()
                .messages
                .last()
                .unwrap()
                .content,
            "window two"
        );
    }

    /// A mutex in one Smithy process is invisible to another open window. The
    /// lease must be represented by the filesystem so an independently opened
    /// descriptor observes contention before either process performs its CAS.
    #[test]
    fn the_store_writer_lease_is_visible_to_independent_file_descriptors() {
        let (_t, store) = store();
        let lease = WriterLease::acquire(store.root()).unwrap();
        let other = OpenOptions::new()
            .read(true)
            .write(true)
            .open(store.root().join(".writer.lock"))
            .unwrap();
        assert!(
            other.try_lock_exclusive().is_err(),
            "a second writer entered while the store lease was held"
        );
        drop(lease);
        other.try_lock_exclusive().unwrap();
        FileExt::unlock(&other).unwrap();
    }

    /// A copied or renamed JSON file must not be able to select an id outside
    /// its own filename, and path-shaped ids must never escape the store.
    #[test]
    fn ids_must_be_safe_single_components_matching_their_file_stems() {
        let (_t, store) = store();
        let stored = StoredSession::from_history(
            "inside",
            Path::new("/tmp"),
            "m",
            &sample_history(),
            &Sampling::default(),
            &Limits::default(),
        );
        std::fs::write(
            store.root().join("renamed.json"),
            serde_json::to_string(&stored).unwrap(),
        )
        .unwrap();

        assert!(store.load("../outside").unwrap_err().contains("safe filename"));
        let listing = store.list_with_warnings().unwrap();
        assert!(listing.sessions.is_empty());
        assert!(listing
            .warnings
            .iter()
            .any(|warning| warning.contains("does not match filename stem")));
    }

    /// A session copied into another project's store can look perfectly valid.
    /// Canonical workspace identity is the dimension that must still refuse it.
    #[test]
    fn copied_sessions_do_not_resume_in_a_different_canonical_workspace() {
        let temp = tempfile::tempdir().unwrap();
        let first = temp.path().join("first");
        let second = temp.path().join("second");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        let schema = serde_json::json!([]);
        let saved =
            SessionBinding::new("local", "http://localhost:1", "m", None, &schema, &first)
                .unwrap();
        let current =
            SessionBinding::new("local", "http://localhost:1", "m", None, &schema, &second)
                .unwrap();
        assert_eq!(
            mismatch_between(saved, current),
            vec![BindingMismatch::Workspace]
        );
    }

    /// Provider display errors may contain response bodies, query credentials,
    /// and userinfo-bearing URLs. The durable failure is a closed category and
    /// status only, so none of those attacker-controlled strings survive.
    #[test]
    fn persisted_failures_exclude_bodies_urls_and_credentials() {
        let failure = PersistedFailure::from_provider_error(&crate::provider::ProviderError::Http {
            status: 401,
            body: "token=secret at https://user:pass@example.test/?key=secret".into(),
        });
        let json = serde_json::to_string(&failure).unwrap();
        assert_eq!(failure.category, PersistedFailureCategory::Authentication);
        for secret in ["token=", "secret", "user:pass", "example.test", "?key="] {
            assert!(!json.contains(secret), "serialized failure leaked {secret}");
        }
    }

    /// Message ancestry alone is insufficient: an older message prefix can
    /// still carry reasoning emitted before a reconnect. Treating it as simply
    /// stale would silently discard the only copy of that sidecar.
    #[test]
    fn a_history_prefix_with_unique_reasoning_forks_instead_of_losing_it() {
        let (_t, store) = store();
        let identity = binding("local", "https://example.test/v1", "m", None, "read");
        let mut existing = bound("reasoning-cas", identity, 2);
        existing.messages.push(Message::assistant("disk continuation"));
        store.save(&existing).unwrap();

        let mut candidate = existing.clone();
        candidate.messages.pop();
        candidate.revision = 3;
        candidate.reasoning.push(ReasoningEntry {
            step: 1,
            after_message: candidate.messages.len(),
            at: 1,
            text: "unique in-memory reasoning".into(),
        });
        let SaveOutcome::Forked { forked, .. } = store.save(&candidate).unwrap() else {
            panic!("opposed history/reasoning ancestry must fork");
        };
        assert!(store
            .load(&forked.id)
            .unwrap()
            .reasoning
            .iter()
            .any(|entry| entry.text == "unique in-memory reasoning"));
        assert!(store
            .load("reasoning-cas")
            .unwrap()
            .messages
            .iter()
            .any(|message| message.content == "disk continuation"));
    }

    /// A candidate may extend History while disk uniquely records why the prior
    /// turn failed. Overwriting that outcome would preserve model bytes but lose
    /// the human-visible failure, so the branches are not append-compatible.
    #[test]
    fn a_history_extension_cannot_erase_a_unique_failed_outcome() {
        let (_t, store) = store();
        let identity = binding("local", "https://example.test/v1", "m", None, "read");
        let mut existing = bound("outcome-cas", identity, 1);
        existing.turn_outcomes.push(TurnOutcomeEntry {
            after_message: existing.messages.len(),
            at: 1,
            status: PersistedTurnStatus::Failed,
            detail: None,
            failure: Some(PersistedFailure {
                category: PersistedFailureCategory::InvalidResponse,
                http_status: None,
                detail: "The provider returned an invalid response.".into(),
            }),
        });
        store.save(&existing).unwrap();

        let mut candidate = existing.clone();
        candidate.turn_outcomes.clear();
        candidate.messages.push(Message::user("continued elsewhere"));
        candidate.revision = 2;
        assert!(matches!(
            store.save(&candidate).unwrap(),
            SaveOutcome::Forked { .. }
        ));
        assert_eq!(
            store.load("outcome-cas").unwrap().turn_outcomes.len(),
            1,
            "the unique failed outcome was overwritten"
        );
    }

    /// Cumulative accounting is append-only per field. Crossed totals mean each
    /// process observed spend the other did not; choosing either would undercount
    /// one dimension, so neither snapshot may replace the other.
    #[test]
    fn crossed_accounting_totals_fork_instead_of_underreporting_usage() {
        let (_t, store) = store();
        let identity = binding("local", "https://example.test/v1", "m", None, "read");
        let mut existing = bound("accounting-cas", identity, 1);
        existing.accounting.usage.prompt_tokens = 100;
        store.save(&existing).unwrap();

        let mut candidate = existing.clone();
        candidate.revision = 2;
        candidate.accounting.usage.prompt_tokens = 0;
        candidate.accounting.usage.completion_tokens = 10;
        assert!(matches!(
            store.save(&candidate).unwrap(),
            SaveOutcome::Forked { .. }
        ));
    }

    /// Repeatedly forking an already-forked maximum-length id used to grow the
    /// suffix until the filesystem rejected it. Conflict names must stay bounded
    /// while retaining a recognizable root and collision-resistant association.
    #[test]
    fn repeated_conflict_ids_remain_bounded_valid_and_associated() {
        let (_t, store) = store();
        let original_id = "a".repeat(MAX_SESSION_ID_BYTES);
        let identity = binding("local", "https://example.test/v1", "m", None, "read");
        let mut original = bound(&original_id, identity, 7);
        original.messages.push(Message::assistant("first branch"));
        store.save(&original).unwrap();

        let mut second = original.clone();
        second.messages.pop();
        second.messages.push(Message::assistant("second branch"));
        let SaveOutcome::Forked { forked, .. } = store.save(&second).unwrap() else {
            panic!("first conflict did not fork");
        };
        assert!(forked.id.len() <= MAX_SESSION_ID_BYTES);
        assert!(forked.id.starts_with(&original_id[..32]));
        validate_session_id(&forked.id).unwrap();

        let mut third = forked.clone();
        third.messages.pop();
        third.messages.push(Message::assistant("third branch"));
        let SaveOutcome::Forked {
            forked: repeated, ..
        } = store.save(&third).unwrap()
        else {
            panic!("repeated conflict did not fork");
        };
        assert!(repeated.id.len() <= MAX_SESSION_ID_BYTES);
        assert!(repeated.id.starts_with(&original_id[..32]));
        validate_session_id(&repeated.id).unwrap();
    }

    /// Delete races the same paths and directory metadata as save. It must wait
    /// behind both the process mutex and the OS-visible writer lease rather than
    /// unlinking a file between CAS inspection and rename.
    #[test]
    fn delete_uses_the_same_process_lock_and_writer_lease_as_save() {
        let (_t, store) = store();
        for id in ["process-locked", "lease-locked"] {
            store
                .save(&StoredSession::from_history(
                    id,
                    Path::new("/tmp"),
                    "m",
                    &sample_history(),
                    &Sampling::default(),
                    &Limits::default(),
                ))
                .unwrap();
        }

        let process_lock = save_lock_for(store.root());
        let process_guard = process_lock.lock().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let deleting = store.clone();
        std::thread::spawn(move || tx.send(deleting.delete("process-locked")).unwrap());
        assert!(rx
            .recv_timeout(std::time::Duration::from_millis(50))
            .is_err());
        drop(process_guard);
        rx.recv_timeout(std::time::Duration::from_secs(1))
            .unwrap()
            .unwrap();

        let lease = WriterLease::acquire(store.root()).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let deleting = store.clone();
        std::thread::spawn(move || tx.send(deleting.delete("lease-locked")).unwrap());
        assert!(rx
            .recv_timeout(std::time::Duration::from_millis(50))
            .is_err());
        drop(lease);
        rx.recv_timeout(std::time::Duration::from_secs(1))
            .unwrap()
            .unwrap();
    }
}

/// Rebuild a transcript from a stored history.
///
/// Restoring the *conversation* without restoring what you can see would leave
/// a session that the model remembers and the user does not — the panel would
/// look empty while the agent silently carried thousands of tokens of context.
///
/// Tool calls and their results are collapsed back into single entries, matched
/// by `tool_call_id` exactly as the live panel does.
pub fn transcript(history: &History) -> Vec<TranscriptEntry> {
    transcript_with_outcomes(history, &[])
}

/// Rebuild the visible transcript, placing stopped/failed sidecars at the
/// provider-message boundary where the turn ended.
pub fn transcript_with_outcomes(
    history: &History,
    outcomes: &[TurnOutcomeEntry],
) -> Vec<TranscriptEntry> {
    use crate::message::Role;

    let mut out = Vec::new();
    // Pending tool calls awaiting their result, in call order.
    let mut pending: Vec<(String, String, String)> = Vec::new();

    append_outcomes(&mut out, outcomes, 0);
    for (index, message) in history.messages().iter().enumerate() {
        match message.role {
            Role::System => {}
            Role::User => {
                // A tool-retry nudge is machinery, not something the user said.
                if !message.content.starts_with("Your previous ") {
                    out.push(TranscriptEntry::User(message.content.clone()));
                }
            }
            Role::Assistant => {
                if message.tool_calls.is_empty() {
                    if !message.content.trim().is_empty() {
                        out.push(TranscriptEntry::Answer(message.content.clone()));
                    }
                } else {
                    for call in &message.tool_calls {
                        pending.push((call.id.clone(), call.name.clone(), call.arguments.clone()));
                    }
                }
            }
            Role::Tool => {
                let id = message.tool_call_id.clone().unwrap_or_default();
                if let Some(pos) = pending.iter().position(|(pid, _, _)| *pid == id) {
                    let (id, name, arguments) = pending.remove(pos);
                    out.push(TranscriptEntry::Step {
                        id,
                        name,
                        arguments,
                        content: message.content.clone(),
                    });
                }
            }
        }
        append_outcomes(&mut out, outcomes, index + 1);
    }

    // A call whose result never arrived — the session ended mid-turn.
    for (id, name, arguments) in pending {
        out.push(TranscriptEntry::Step {
            id,
            name,
            arguments,
            content: "[no result recorded]".into(),
        });
    }

    out
}

fn append_outcomes(
    out: &mut Vec<TranscriptEntry>,
    outcomes: &[TurnOutcomeEntry],
    after_message: usize,
) {
    for outcome in outcomes
        .iter()
        .filter(|outcome| outcome.after_message == after_message)
    {
        match outcome.status {
            PersistedTurnStatus::Answered => {}
            PersistedTurnStatus::Stopped => out.push(TranscriptEntry::Stopped(
                outcome
                    .detail
                    .clone()
                    .unwrap_or_else(|| "Turn stopped.".into()),
            )),
            PersistedTurnStatus::Failed => out.push(TranscriptEntry::Failed(
                outcome
                    .failure
                    .as_ref()
                    .map(|failure| failure.detail.clone())
                    .unwrap_or_else(|| "The turn failed.".into()),
            )),
        }
    }
}

/// One restored transcript entry, in the shape a UI needs.
#[derive(Debug, Clone, PartialEq)]
pub enum TranscriptEntry {
    User(String),
    Answer(String),
    Stopped(String),
    Failed(String),
    Step {
        id: String,
        name: String,
        arguments: String,
        content: String,
    },
}

#[cfg(test)]
mod transcript_tests {
    use super::*;
    use crate::message::Message;
    use smithy_tools::{ToolCall, ToolResult};

    fn history() -> History {
        let mut h = History::new("system");
        h.push(Message::user("read notes.txt"));
        let call = ToolCall::new("c1", "read", r#"{"path":"notes.txt"}"#);
        h.push(Message::assistant_with_calls("", vec![call.clone()]));
        h.push(Message::tool_result(&ToolResult::ok(&call, "FJORD")));
        h.push(Message::assistant("The file says FJORD."));
        h
    }

    #[test]
    fn the_system_prompt_is_not_part_of_the_transcript() {
        let entries = transcript(&history());
        assert!(!entries
            .iter()
            .any(|e| matches!(e, TranscriptEntry::User(t) if t == "system")));
    }

    #[test]
    fn calls_and_results_collapse_into_one_step() {
        let entries = transcript(&history());
        assert_eq!(entries.len(), 3);
        match &entries[1] {
            TranscriptEntry::Step {
                id, name, content, ..
            } => {
                assert_eq!(id, "c1");
                assert_eq!(name, "read");
                assert_eq!(content, "FJORD");
            }
            other => panic!("expected a step, got {other:?}"),
        }
    }

    #[test]
    fn the_order_of_the_conversation_is_preserved() {
        let entries = transcript(&history());
        assert!(matches!(&entries[0], TranscriptEntry::User(t) if t == "read notes.txt"));
        assert!(matches!(&entries[2], TranscriptEntry::Answer(t) if t.contains("FJORD")));
    }

    /// Stopped and failed turns used to disappear after relaunch because they
    /// deliberately do not enter provider History. Their `after_message`
    /// boundary must restore them at the same visible point without changing
    /// any replayed message.
    #[test]
    fn stopped_and_failed_outcomes_restore_at_their_message_boundaries() {
        let history = history();
        let outcomes = vec![
            TurnOutcomeEntry {
                after_message: 2,
                at: 1,
                status: PersistedTurnStatus::Stopped,
                detail: Some("cancelled".into()),
                failure: None,
            },
            TurnOutcomeEntry {
                after_message: history.len(),
                at: 2,
                status: PersistedTurnStatus::Failed,
                detail: None,
                failure: Some(PersistedFailure {
                    category: PersistedFailureCategory::InvalidResponse,
                    http_status: None,
                    detail: "The provider returned an invalid response.".into(),
                }),
            },
        ];
        let entries = transcript_with_outcomes(&history, &outcomes);
        assert!(entries
            .iter()
            .any(|entry| matches!(entry, TranscriptEntry::Stopped(text) if text == "cancelled")));
        assert!(matches!(
            entries.last(),
            Some(TranscriptEntry::Failed(text)) if text.contains("invalid response")
        ));
        assert_eq!(history.messages()[0].content, "system");
    }

    /// Parallel calls must each keep their own result across a restore, exactly
    /// as they do live.
    #[test]
    fn parallel_calls_keep_their_own_results() {
        let mut h = History::new("system");
        h.push(Message::user("do both"));
        let a = ToolCall::new("call_a", "read", "{}");
        let b = ToolCall::new("call_b", "grep", "{}");
        h.push(Message::assistant_with_calls(
            "",
            vec![a.clone(), b.clone()],
        ));
        // Results out of order, as they arrive in practice.
        h.push(Message::tool_result(&ToolResult::ok(&b, "B result")));
        h.push(Message::tool_result(&ToolResult::ok(&a, "A result")));

        let entries = transcript(&h);
        for entry in &entries {
            if let TranscriptEntry::Step { id, content, .. } = entry {
                match id.as_str() {
                    "call_a" => assert_eq!(content, "A result"),
                    "call_b" => assert_eq!(content, "B result"),
                    other => panic!("unexpected id {other}"),
                }
            }
        }
    }

    /// A session killed mid-turn leaves a call with no result. It should still
    /// appear, marked, rather than vanishing.
    #[test]
    fn a_call_without_a_result_is_still_shown() {
        let mut h = History::new("system");
        h.push(Message::user("go"));
        h.push(Message::assistant_with_calls(
            "",
            vec![ToolCall::new("orphan", "bash", "{}")],
        ));
        let entries = transcript(&h);
        assert!(entries.iter().any(|e| matches!(
            e,
            TranscriptEntry::Step { id, content, .. }
                if id == "orphan" && content.contains("no result")
        )));
    }

    /// Retry nudges are machinery the loop injected, not something the user
    /// typed; replaying them as user messages would be a lie.
    #[test]
    fn retry_nudges_are_not_shown_as_user_messages() {
        let mut h = History::new("system");
        h.push(Message::user("real question"));
        h.push(Message::assistant(""));
        h.push(Message::user(
            "Your previous response was cut off by the token limit. Give ONLY the final answer.",
        ));
        h.push(Message::assistant("the answer"));

        let users: Vec<&String> = transcript(&h)
            .iter()
            .filter_map(|e| match e {
                TranscriptEntry::User(t) => Some(t),
                _ => None,
            })
            .cloned()
            .collect::<Vec<String>>()
            .leak()
            .iter()
            .collect();
        assert_eq!(users.len(), 1);
        assert_eq!(users[0], "real question");
    }

    #[test]
    fn an_empty_history_yields_an_empty_transcript() {
        assert!(transcript(&History::new("system")).is_empty());
    }
}
