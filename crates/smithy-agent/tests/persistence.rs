//! Saving a session, loading it, resuming it, and continuing it.
//!
//! There is already a unit test asserting the history round-trips byte-identically.
//! What was never tested is doing it through a real `Session`: run a turn, save,
//! load from disk, resume, run another turn, and check that nothing the first turn
//! produced moved by a byte.
//!
//! That is not tidiness. Prefix caching on a local endpoint is a strict prefix
//! match, so one changed byte anywhere in the replayed history reverts to a full
//! cold prefill — minutes at real context sizes. A resumed session that drifts
//! costs that on every subsequent turn, and the symptom is "resuming is slow"
//! rather than anything that looks like a correctness bug.
//!
//! Everything here asserts observable state: bytes on disk, message contents, what
//! `list` returns. Nothing asserts on the model's prose.

use std::sync::Arc;

use smithy_agent::persist::StoredSession;
use smithy_agent::provider::test_support::{answer, tool_call, ScriptedProvider};
use smithy_agent::{Completion, History, Limits, Sampling, Session, SessionConfig, SessionStore};
use smithy_tools::{Registry, ToolCtx, Workspace};

struct Fixture {
    _root: tempfile::TempDir,
    store: SessionStore,
    ctx: Arc<ToolCtx>,
    workspace_root: std::path::PathBuf,
}

fn fixture() -> Fixture {
    let root = tempfile::tempdir().expect("tempdir");

    let work = root.path().join("workspace");
    std::fs::create_dir_all(&work).expect("workspace dir");
    std::fs::write(work.join("notes.txt"), "alpha\nbravo\n").expect("seed");

    let store = SessionStore::new(root.path().join("sessions")).expect("store");
    let ctx = Arc::new(ToolCtx::new(Workspace::open(&work).expect("workspace")));

    Fixture {
        _root: root,
        store,
        ctx,
        workspace_root: work,
    }
}

fn session(f: &Fixture, script: Vec<Completion>) -> Session {
    Session::new(
        Arc::new(ScriptedProvider::new(script)),
        Arc::new(Registry::core()),
        f.ctx.clone(),
        SessionConfig::new("you are a test"),
    )
}

fn resumed(f: &Fixture, stored: StoredSession, script: Vec<Completion>) -> Session {
    let sampling = stored.sampling.clone();
    let limits = stored.limits.clone();
    let skill = stored.skill_name();
    let tools = stored.tools.clone();
    let mut session = Session::resume(
        Arc::new(ScriptedProvider::new(script)),
        Arc::new(Registry::core()),
        f.ctx.clone(),
        stored.into_history(),
        sampling,
        limits,
        skill,
    );
    if let Some(tools) = tools {
        session.freeze_tools(tools);
    }
    session
}

fn snapshot(history: &History) -> Vec<String> {
    history
        .messages()
        .iter()
        .map(|m| serde_json::to_string(m).expect("serialisable"))
        .collect()
}

fn store_from(f: &Fixture, id: &str, s: &Session) -> StoredSession {
    StoredSession::from_history_with_reasoning(
        id,
        &f.workspace_root,
        "test-model",
        s.history(),
        s.sampling(),
        s.limits(),
        s.reasoning().to_vec(),
        s.skill().map(str::to_string),
    )
    .with_tools(s.tools_schema().clone())
}

// ---------------------------------------------------------------------------
// The cycle
// ---------------------------------------------------------------------------

/// The whole point: a session that has been through disk and back must continue
/// from byte-identical history, or every turn after the resume pays a cold
/// prefill.
#[tokio::test]
async fn a_resumed_session_continues_from_byte_identical_history() {
    let f = fixture();

    let mut first = session(
        &f,
        vec![
            tool_call("c1", "read", r#"{"path": "notes.txt"}"#),
            answer("it says alpha and bravo"),
        ],
    );
    first
        .run_turn("what is in notes.txt?", None)
        .await
        .expect("turn one");

    let before = snapshot(first.history());
    f.store
        .save(&store_from(&f, "sess-1", &first))
        .expect("save");

    // A genuinely separate load, not the object still in memory.
    let stored = f.store.load("sess-1").expect("load");
    let mut second = resumed(&f, stored, vec![answer("still here")]);

    assert_eq!(
        snapshot(second.history()),
        before,
        "the resumed history differs from what was saved"
    );

    second
        .run_turn("are you still there?", None)
        .await
        .expect("turn two");

    let after = snapshot(second.history());
    assert!(
        after.len() > before.len(),
        "the second turn appended nothing"
    );
    for (i, msg) in before.iter().enumerate() {
        assert_eq!(
            msg, &after[i],
            "message {i} changed after resuming — the cached prefix is invalidated"
        );
    }
}

/// Tool results are part of the prefix too. A resume that dropped or re-rendered
/// them would leave tool calls without matching results, which the endpoint
/// rejects outright rather than degrading gracefully.
#[tokio::test]
async fn tool_calls_and_their_results_both_survive_a_round_trip() {
    let f = fixture();

    let mut s = session(
        &f,
        vec![
            tool_call("call-abc", "read", r#"{"path": "notes.txt"}"#),
            answer("done"),
        ],
    );
    s.run_turn("read it", None).await.expect("turn");

    f.store.save(&store_from(&f, "sess-1", &s)).expect("save");
    let reloaded = f.store.load("sess-1").expect("load").into_history();

    let json = reloaded.to_api().to_string();
    assert!(
        json.contains("call-abc"),
        "the call id must survive, or its result cannot be matched to it: {json}"
    );
    assert!(
        json.contains("alpha"),
        "the tool's output must survive: {json}"
    );

    let roles: Vec<_> = reloaded.messages().iter().map(|m| m.role).collect();
    assert!(
        roles.contains(&smithy_agent::Role::Tool),
        "a tool result message is missing entirely: {roles:?}"
    );
}

/// The system prompt is the head of the cached prefix. If a round trip moves or
/// rewrites it, nothing downstream can hit the cache.
#[tokio::test]
async fn the_system_prompt_survives_a_round_trip_unchanged() {
    let f = fixture();
    let mut s = session(&f, vec![answer("hello")]);
    s.run_turn("hi", None).await.expect("turn");

    let original = s.history().system_prompt().map(str::to_owned);
    f.store.save(&store_from(&f, "sess-1", &s)).expect("save");
    let reloaded = f.store.load("sess-1").expect("load").into_history();

    assert_eq!(original.as_deref(), reloaded.system_prompt());
    assert_eq!(
        reloaded.messages()[0].role,
        smithy_agent::Role::System,
        "the system message must still be first"
    );
}

/// Saving twice under one id appends to the same session rather than forking a
/// second copy — which is what the UI does after every turn.
#[tokio::test]
async fn saving_the_same_id_twice_updates_rather_than_duplicating() {
    let f = fixture();
    let mut s = session(&f, vec![answer("one"), answer("two")]);

    s.run_turn("first", None).await.expect("turn one");
    f.store
        .save(&store_from(&f, "sess-1", &s))
        .expect("save one");

    s.run_turn("second", None).await.expect("turn two");
    f.store
        .save(&store_from(&f, "sess-1", &s))
        .expect("save two");

    let listed = f.store.list().expect("list");
    assert_eq!(
        listed.len(),
        1,
        "a second save forked the session: {listed:?}"
    );

    let loaded = f.store.load("sess-1").expect("load");
    let text = serde_json::to_string(&loaded.messages).expect("serialise");
    assert!(text.contains("first") && text.contains("second"), "{text}");
}

// ---------------------------------------------------------------------------
// The list, which is what the user actually sees
// ---------------------------------------------------------------------------

/// One corrupt file must not take the list with it. The list is how a project's
/// history is reached at all, so failing it whole loses every other session.
#[tokio::test]
async fn a_corrupt_session_file_does_not_break_the_list() {
    let f = fixture();

    for (id, prompt) in [("good-1", "first"), ("good-2", "second")] {
        let mut s = session(&f, vec![answer("ok")]);
        s.run_turn(prompt, None).await.expect("turn");
        f.store.save(&store_from(&f, id, &s)).expect("save");
    }

    std::fs::write(f.store.root().join("broken.json"), "{ this is not json")
        .expect("write corrupt file");

    let listed = f.store.list().expect("list still succeeds");
    let ids: Vec<_> = listed.iter().map(|s| s.id.as_str()).collect();
    assert!(ids.contains(&"good-1"), "{ids:?}");
    assert!(ids.contains(&"good-2"), "{ids:?}");
    assert!(
        !ids.contains(&"broken"),
        "the corrupt file must be skipped, not surfaced: {ids:?}"
    );
}

/// Loading a session that is not there is an error, not a panic and not an empty
/// session silently pretending to be the one asked for.
#[tokio::test]
async fn loading_a_missing_session_is_an_error() {
    let f = fixture();
    assert!(f.store.load("nope").is_err());
}

/// The title is what a session list is navigated by. Derived from the first user
/// message, and it has to survive the round trip or every row reads the same.
#[tokio::test]
async fn the_title_comes_from_the_first_user_message() {
    let f = fixture();
    let mut s = session(&f, vec![answer("ok"), answer("ok")]);

    s.run_turn("refactor the parser", None)
        .await
        .expect("turn one");
    s.run_turn("now the lexer", None).await.expect("turn two");
    f.store.save(&store_from(&f, "sess-1", &s)).expect("save");

    let loaded = f.store.load("sess-1").expect("load");
    assert_eq!(
        loaded.title, "refactor the parser",
        "the title must name the session, not its latest turn"
    );
}

/// Deleting a session removes it and leaves the others alone.
#[tokio::test]
async fn deleting_a_session_leaves_the_others() {
    let f = fixture();
    for id in ["a", "b"] {
        let mut s = session(&f, vec![answer("ok")]);
        s.run_turn(id, None).await.expect("turn");
        f.store.save(&store_from(&f, id, &s)).expect("save");
    }

    f.store.delete("a").expect("delete");

    let ids: Vec<_> = f
        .store
        .list()
        .expect("list")
        .into_iter()
        .map(|s| s.id)
        .collect();
    assert_eq!(ids, vec!["b".to_string()]);
    assert!(f.store.load("a").is_err(), "a deleted session must be gone");
}

// ---------------------------------------------------------------------------
// Format
// ---------------------------------------------------------------------------

/// The schema version is stamped so a later format change can migrate rather
/// than silently misread an old file as a valid new one.
#[tokio::test]
async fn a_saved_session_records_its_schema_version() {
    let f = fixture();
    let mut s = session(&f, vec![answer("ok")]);
    s.run_turn("hi", None).await.expect("turn");
    f.store.save(&store_from(&f, "sess-1", &s)).expect("save");

    let raw = std::fs::read_to_string(f.store.root().join("sess-1.json")).expect("read raw");
    let parsed: serde_json::Value = serde_json::from_str(&raw).expect("valid json");
    assert_eq!(
        parsed["version"],
        serde_json::json!(smithy_agent::persist::SCHEMA_VERSION)
    );
}

/// Resume must POST the stored tool JSON even if the live registry no longer
/// has those names (an MCP server that is down).
#[tokio::test]
async fn resume_sends_stored_tools_even_if_the_live_registry_moved() {
    let f = fixture();
    let mut first = session(&f, vec![answer("one")]);
    first.run_turn("hi", None).await.expect("turn");
    let stored_tools = first.tools_schema().clone();
    f.store
        .save(&store_from(&f, "sess-1", &first))
        .expect("save");

    let stored = f.store.load("sess-1").expect("load");
    let tools = stored.tools.clone().expect("tools were saved");
    let mut registry = Registry::new().with(smithy_tools::tools::read::Read);
    smithy_agent::mcp::stub_unavailable(&mut registry, &tools);
    let mut second = Session::resume(
        Arc::new(ScriptedProvider::new(vec![answer("two")])),
        Arc::new(registry),
        f.ctx.clone(),
        stored.into_history(),
        Sampling::default(),
        Limits::default(),
        None,
    );
    second.freeze_tools(tools);
    second.run_turn("again", None).await.expect("turn");
    let body = second.last_request().expect("posted");
    assert_eq!(&body["tools"], &stored_tools);
}

/// Sampling and limits travel with the session, so resuming reproduces the run
/// rather than quietly adopting whatever the current defaults happen to be.
#[tokio::test]
async fn sampling_and_limits_are_restored_not_defaulted() {
    let f = fixture();

    let distinctive = Limits {
        max_steps: 7,
        context_hard: 12_345,
        ..Default::default()
    };
    let sampling = Sampling::default();

    let mut s = session(&f, vec![answer("ok")]);
    s.run_turn("hi", None).await.expect("turn");

    let stored = StoredSession::from_history(
        "sess-1",
        &f.workspace_root,
        "test-model",
        s.history(),
        &sampling,
        &distinctive,
    );
    f.store.save(&stored).expect("save");

    let loaded = f.store.load("sess-1").expect("load");
    assert_eq!(loaded.limits.max_steps, 7);
    assert_eq!(loaded.limits.context_hard, 12_345);

    let resumed_session = resumed(&f, loaded, vec![answer("ok")]);
    assert_eq!(resumed_session.limits().max_steps, 7);
    assert_eq!(resumed_session.limits().context_hard, 12_345);
}
