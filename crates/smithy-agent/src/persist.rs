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

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::limits::Limits;
use crate::message::{History, Message};
use crate::provider::Sampling;

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
    /// A short label for a session list. Derived from the first user message.
    pub title: String,
    pub sampling: Sampling,
    pub limits: Limits,
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
    /// Last Skill invoked in this Session, if any. None means none yet.
    ///
    /// `#[serde(default)]` so sessions written before skills existed still load.
    #[serde(default)]
    pub skill: Option<String>,
    /// Written when Research and Grill were harness kinds. New saves leave this
    /// as Coding; [`Self::skill_name`] still reads the old values.
    #[serde(default)]
    kind: LegacyKind,
    /// Frozen OpenAI `tools` array. Resume sends these bytes even if an MCP
    /// server is down; execute then errors for missing names. Absent on
    /// sessions written before this field existed — those recompute from the
    /// live registry, as before.
    #[serde(default)]
    pub tools: Option<Value>,
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

pub const SCHEMA_VERSION: u32 = 2;

/// Sessions written when `/research` and `/grill-me` were Session kinds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum LegacyKind {
    #[default]
    Coding,
    Research,
    Grill,
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
        Self::from_history_with_reasoning(
            id,
            workspace,
            model,
            history,
            sampling,
            limits,
            Vec::new(),
            None,
        )
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
        skill: Option<String>,
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
            title: derive_title(&messages),
            sampling: sampling.clone(),
            limits: limits.clone(),
            messages,
            reasoning,
            skill,
            kind: LegacyKind::Coding,
            tools: None,
        }
    }

    /// Last Skill invoked in this Session. Migrates files that only have `kind`.
    pub fn skill_name(&self) -> Option<String> {
        if let Some(name) = &self.skill {
            if !name.is_empty() {
                return Some(name.clone());
            }
        }
        match self.kind {
            LegacyKind::Research => Some("research".into()),
            LegacyKind::Grill => Some("grill-me".into()),
            LegacyKind::Coding => None,
        }
    }

    /// Pin the tool JSON that was advertised when this Session was built.
    pub fn with_tools(mut self, tools: Value) -> StoredSession {
        self.tools = Some(tools);
        self
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

/// A directory of stored sessions, one JSON file each.
///
/// One file per session rather than a database: sessions are written whole and
/// read whole, never queried, and a plain file the user can read, diff, or
/// delete with `rm` is worth more here than indexed access.
pub struct SessionStore {
    root: PathBuf,
}

impl SessionStore {
    pub fn new(root: impl Into<PathBuf>) -> Result<SessionStore, String> {
        let root = root.into();
        std::fs::create_dir_all(&root)
            .map_err(|e| format!("cannot create session store at {}: {e}", root.display()))?;
        Ok(SessionStore { root })
    }

    /// Fallback location: `~/.local/share/smithy/sessions`.
    ///
    /// That is the XDG data directory. The app uses it on macOS as well, so a
    /// single path works everywhere this crate might run. The editor itself
    /// stores conversations under `~/.local/share/smithy/projects/<project>/sessions`;
    /// this method is the crate-level default for non-UI consumers.
    pub fn default_location() -> Result<SessionStore, String> {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .ok_or("HOME is not set; pass an explicit session store path")?;
        SessionStore::new(home.join(".local/share/smithy/sessions"))
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn path_for(&self, id: &str) -> PathBuf {
        self.root.join(format!("{id}.json"))
    }

    /// Live Session file.
    pub fn session_path(&self, id: &str) -> PathBuf {
        self.path_for(id)
    }

    /// Full pre-compact log, if Compact archived one; otherwise the live file.
    pub fn log_path(&self, id: &str) -> PathBuf {
        let full = self.full_log_path(id);
        if full.is_file() {
            full
        } else {
            self.path_for(id)
        }
    }

    fn full_log_path(&self, id: &str) -> PathBuf {
        self.root.join(format!("{id}.full.json"))
    }

    /// Copy the live file to `{id}.full.json` once, so Compact can overwrite
    /// the Session without destroying the only log.
    pub fn archive_full(&self, id: &str) -> Result<(), String> {
        let src = self.path_for(id);
        if !src.is_file() {
            return Ok(());
        }
        let dst = self.full_log_path(id);
        if dst.is_file() {
            return Ok(());
        }
        std::fs::copy(&src, &dst).map_err(|e| format!("cannot archive session {id}: {e}"))?;
        Ok(())
    }

    /// Write a session, preserving when it was first created.
    ///
    /// `created_at` comes from whatever is already on disk under this id, not
    /// from the value handed in. The caller builds a `StoredSession` from the
    /// live history on every save — `StoredSession::from_history` stamps *now* —
    /// so trusting the argument meant `created_at` was rewritten on every turn
    /// and a session's age was always zero. Reading it back here fixes it for
    /// every caller rather than for the one that happened to be wrong.
    pub fn save(&self, session: &StoredSession) -> Result<(), String> {
        let mut session = session.clone();
        session.updated_at = unix_seconds();
        if let Ok(existing) = self.load(&session.id) {
            session.created_at = existing.created_at;
        }

        let json = serde_json::to_string_pretty(&session)
            .map_err(|e| format!("cannot serialize session {}: {e}", session.id))?;

        // Write to a temporary file and rename, so an interrupted write cannot
        // leave a half-written session that fails to parse on next launch.
        let final_path = self.path_for(&session.id);
        let tmp_path = final_path.with_extension("json.tmp");
        std::fs::write(&tmp_path, &json)
            .map_err(|e| format!("cannot write {}: {e}", tmp_path.display()))?;
        std::fs::rename(&tmp_path, &final_path)
            .map_err(|e| format!("cannot finalize {}: {e}", final_path.display()))?;
        Ok(())
    }

    pub fn load(&self, id: &str) -> Result<StoredSession, String> {
        let path = self.path_for(id);
        let text =
            std::fs::read_to_string(&path).map_err(|e| format!("cannot read session {id}: {e}"))?;
        let session: StoredSession = serde_json::from_str(&text).map_err(|e| {
            format!("session {id} is not readable ({e}); it may be from an older version")
        })?;
        if session.version > SCHEMA_VERSION {
            return Err(format!(
                "session {id} was written by a newer version of Smithy (format {} > {SCHEMA_VERSION})",
                session.version
            ));
        }
        Ok(session)
    }

    pub fn delete(&self, id: &str) -> Result<(), String> {
        let path = self.path_for(id);
        if path.is_file() {
            std::fs::remove_file(&path).map_err(|e| format!("cannot delete session {id}: {e}"))?;
        }
        let full = self.full_log_path(id);
        if full.is_file() {
            let _ = std::fs::remove_file(full);
        }
        Ok(())
    }

    /// Every stored session, most recently updated first.
    pub fn list(&self) -> Result<Vec<StoredSession>, String> {
        let entries = std::fs::read_dir(&self.root)
            .map_err(|e| format!("cannot list {}: {e}", self.root.display()))?;

        let mut sessions = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if !name.ends_with(".json") || name.ends_with(".full.json") || name.ends_with(".tmp") {
                continue;
            }
            // A single corrupt file should not hide every other session.
            if let Ok(text) = std::fs::read_to_string(&path) {
                if let Ok(session) = serde_json::from_str::<StoredSession>(&text) {
                    sessions.push(session);
                }
            }
        }
        sessions.sort_by_key(|s| std::cmp::Reverse(s.updated_at));
        Ok(sessions)
    }
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
    fn a_full_log_sidecar_is_not_listed_as_a_session() {
        let (_t, store) = store();
        store
            .save(&StoredSession::from_history(
                "s1",
                Path::new("/tmp"),
                "m",
                &sample_history(),
                &Sampling::default(),
                &Limits::default(),
            ))
            .unwrap();
        store.archive_full("s1").unwrap();
        assert!(store.log_path("s1").file_name().unwrap() == "s1.full.json");
        let ids: Vec<String> = store.list().unwrap().into_iter().map(|s| s.id).collect();
        assert_eq!(ids, vec!["s1"]);
        store.archive_full("s1").unwrap();
        assert_eq!(store.list().unwrap().len(), 1);
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

    #[test]
    fn a_newer_schema_version_is_refused_rather_than_misread() {
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
        assert_eq!(files, vec!["s1.json".to_string()]);
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
    fn skill_name_round_trips_instead_of_collapsing_to_coding() {
        let (_t, store) = store();
        let stored = StoredSession::from_history_with_reasoning(
            "s1",
            Path::new("/tmp"),
            "m",
            &sample_history(),
            &Sampling::default(),
            &Limits::default(),
            Vec::new(),
            Some("research".into()),
        );
        store.save(&stored).unwrap();
        assert_eq!(
            store.load("s1").unwrap().skill_name().as_deref(),
            Some("research")
        );
    }

    #[test]
    fn a_legacy_kind_field_still_names_the_skill() {
        let (_t, store) = store();
        let stored = StoredSession::from_history(
            "s1",
            Path::new("/tmp"),
            "m",
            &sample_history(),
            &Sampling::default(),
            &Limits::default(),
        );
        let mut value = serde_json::to_value(&stored).unwrap();
        value.as_object_mut().unwrap().remove("skill");
        value["kind"] = serde_json::json!("grill");
        std::fs::write(
            store.root().join("s1.json"),
            serde_json::to_string(&value).unwrap(),
        )
        .unwrap();
        assert_eq!(
            store.load("s1").unwrap().skill_name().as_deref(),
            Some("grill-me")
        );
    }

    #[test]
    fn frozen_tools_round_trip_instead_of_being_recomputed() {
        let (_t, store) = store();
        let tools = serde_json::json!([{"type":"function","function":{"name":"github_get_me"}}]);
        let stored = StoredSession::from_history(
            "s1",
            Path::new("/tmp"),
            "m",
            &sample_history(),
            &Sampling::default(),
            &Limits::default(),
        )
        .with_tools(tools.clone());
        store.save(&stored).unwrap();
        assert_eq!(store.load("s1").unwrap().tools, Some(tools));
    }

    #[test]
    fn sessions_written_before_tools_still_load() {
        let (_t, store) = store();
        let stored = StoredSession::from_history(
            "s1",
            Path::new("/tmp"),
            "m",
            &sample_history(),
            &Sampling::default(),
            &Limits::default(),
        );
        let mut value = serde_json::to_value(&stored).unwrap();
        value.as_object_mut().unwrap().remove("tools");
        value["version"] = serde_json::json!(1);
        std::fs::write(
            store.root().join("s1.json"),
            serde_json::to_string(&value).unwrap(),
        )
        .unwrap();
        let loaded = store.load("s1").unwrap();
        assert!(loaded.tools.is_none());
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
    use crate::message::Role;

    let mut out = Vec::new();
    // Pending tool calls awaiting their result, in call order.
    let mut pending: Vec<(String, String, String)> = Vec::new();

    for message in history.messages() {
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

/// One restored transcript entry, in the shape a UI needs.
#[derive(Debug, Clone, PartialEq)]
pub enum TranscriptEntry {
    User(String),
    Answer(String),
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
