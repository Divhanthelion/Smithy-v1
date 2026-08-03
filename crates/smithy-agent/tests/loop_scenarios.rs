//! The agent loop, driven end to end against a scripted provider.
//!
//! Every piece of the loop has unit tests. What none of them exercise is a whole
//! turn: provider → parse → dispatch → tool result → provider again, over a real
//! `Registry` and a real sandboxed workspace, with only the model faked.
//!
//! No endpoint and no model. `ScriptedProvider` replays a fixed list of
//! completions, so every scenario here is deterministic and runs in milliseconds —
//! which is the point. A test that needs LM Studio running is a test nobody runs.
//!
//! The scenarios are the six ways this loop has actually gone wrong: multi-turn,
//! parallel tool calls, hook denial, budget exhaustion, malformed→recovery,
//! empty→recovery.
//!
//! **Every assertion here is about observable state** — history contents, tool
//! results, the `Outcome`, files on disk. Never about the model's prose, and never
//! about its reasoning channel. Asserting on generated text makes a test that
//! fails when the model is merely differently right, which teaches you to
//! delete it.

use std::sync::Arc;

use smithy_agent::provider::test_support::{
    answer, answer_at, empty, tool_call, tool_call_at, tool_calls, truncated, ScriptedProvider,
};
use smithy_agent::{Completion, Limits, Outcome, Session, SessionConfig, TurnEvent};
use smithy_tools::{HookDecision, Registry, ToolCall, ToolCtx, ToolHook, Workspace};

/// A session over a real temp workspace, with the model scripted.
struct Harness {
    _root: tempfile::TempDir,
    session: Session,
    provider: Arc<ScriptedProvider>,
    ctx: Arc<ToolCtx>,
}

fn harness_with(script: Vec<Completion>, registry: Registry, limits: Limits) -> Harness {
    let root = tempfile::tempdir().expect("tempdir");
    std::fs::write(root.path().join("notes.txt"), "alpha\nbravo\n").expect("seed");

    let workspace = Workspace::open(root.path()).expect("workspace");
    let ctx = Arc::new(ToolCtx::new(workspace));
    let provider = Arc::new(ScriptedProvider::new(script));

    let mut config = SessionConfig::new("you are a test");
    config.limits = limits;

    let session = Session::new(provider.clone(), Arc::new(registry), ctx.clone(), config);

    Harness {
        _root: root,
        session,
        provider,
        ctx,
    }
}

fn harness(script: Vec<Completion>) -> Harness {
    harness_with(script, Registry::core(), Limits::default())
}

/// Collect the events a turn emits, so ordering and pairing can be asserted.
fn recording_sink() -> (
    Arc<std::sync::Mutex<Vec<TurnEvent>>>,
    impl Fn(TurnEvent) + Send + Sync,
) {
    let log = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink_log = log.clone();
    (log, move |event| sink_log.lock().unwrap().push(event))
}

/// Hook that refuses one named tool, for exercising denial without needing the UI.
struct DenyTool(&'static str);

#[async_trait::async_trait]
impl ToolHook for DenyTool {
    fn name(&self) -> &'static str {
        "deny-tool"
    }
    async fn before(
        &self,
        call: &ToolCall,
        _args: &serde_json::Value,
        _ctx: &ToolCtx,
    ) -> HookDecision {
        if call.name == self.0 {
            HookDecision::Deny("policy forbids this".into())
        } else {
            HookDecision::Allow
        }
    }
}

// ---------------------------------------------------------------------------
// The ordinary shape of a turn
// ---------------------------------------------------------------------------

/// A tool call, its result, then an answer — with the result actually reaching
/// the model, which is the part that cannot be checked by testing the pieces.
#[tokio::test]
async fn a_tool_result_is_fed_back_and_the_turn_answers() {
    let mut h = harness(vec![
        tool_call("c1", "read", r#"{"path": "notes.txt"}"#),
        answer("the file says alpha and bravo"),
    ]);

    let outcome = h.session.run_turn("what is in notes.txt?", None).await;

    assert!(matches!(outcome, Ok(Outcome::Answer(_))));
    assert_eq!(h.provider.call_count(), 2, "one call per model turn");

    let history = h.session.history().to_api().to_string();
    assert!(
        history.contains("alpha"),
        "the tool's output must reach the model, not just the UI: {history}"
    );
}

/// Tool results are matched to their calls by **id**. Two calls to the same tool
/// are otherwise indistinguishable, and results do not arrive in call order.
#[tokio::test]
async fn parallel_calls_to_one_tool_get_their_own_results() {
    let mut h = harness(vec![
        tool_calls(&[
            ("first", "read", r#"{"path": "notes.txt"}"#),
            ("second", "read", r#"{"path": "missing.txt"}"#),
        ]),
        answer("one worked, one did not"),
    ]);

    let (events, sink) = recording_sink();
    let outcome = h.session.run_turn("read both", Some(&sink)).await;
    assert!(matches!(outcome, Ok(Outcome::Answer(_))));

    let finished: Vec<(String, bool)> = events
        .lock()
        .unwrap()
        .iter()
        .filter_map(|e| match e {
            TurnEvent::ToolFinished { id, is_error, .. } => Some((id.clone(), *is_error)),
            _ => None,
        })
        .collect();

    assert_eq!(finished.len(), 2, "both calls resolve: {finished:?}");
    assert!(
        finished.contains(&("first".to_string(), false)),
        "the readable file succeeded under its own id: {finished:?}"
    );
    assert!(
        finished.contains(&("second".to_string(), true)),
        "the missing file failed under its own id: {finished:?}"
    );
}

/// Every announced call must produce a result. A tool call with no matching
/// result is a conversation the endpoint rejects outright.
#[tokio::test]
async fn every_announced_call_has_a_result_in_history() {
    let mut h = harness(vec![
        tool_calls(&[
            ("a", "read", r#"{"path": "notes.txt"}"#),
            ("b", "ls", r#"{"path": "."}"#),
            ("c", "read", r#"{"path": "nope.txt"}"#),
        ]),
        answer("done"),
    ]);

    h.session.run_turn("go", None).await.expect("turn runs");

    let history = h.session.history().to_api().to_string();
    for id in ["a", "b", "c"] {
        assert!(
            history.contains(id),
            "call `{id}` has no result in history: {history}"
        );
    }
}

// ---------------------------------------------------------------------------
// Recovery
// ---------------------------------------------------------------------------

/// An empty answer is a failure, not a finish. At high context the model reasons
/// correctly but loops in its reasoning channel until the token limit, emitting
/// nothing — accepting that produces a silent no-op turn.
#[tokio::test]
async fn an_empty_answer_is_retried_rather_than_accepted() {
    let mut h = harness(vec![empty(), answer("actually, here it is")]);

    let outcome = h.session.run_turn("say something", None).await;

    match outcome {
        Ok(Outcome::Answer(text)) => assert_eq!(text.trim(), "actually, here it is"),
        other => panic!("expected the retry to answer, got {other:?}"),
    }
    assert_eq!(
        h.provider.call_count(),
        2,
        "the empty response must have been retried"
    );
}

/// A response cut off by the token limit is also not a finish.
#[tokio::test]
async fn a_truncated_answer_is_retried_rather_than_accepted() {
    let mut h = harness(vec![
        truncated("here is the first half of a sent"),
        answer("a complete answer"),
    ]);

    let outcome = h.session.run_turn("explain", None).await;

    match outcome {
        Ok(Outcome::Answer(text)) => assert_eq!(text.trim(), "a complete answer"),
        other => panic!("expected the retry to answer, got {other:?}"),
    }
}

/// Repeated unusable responses stop the turn rather than looping forever. The
/// distinction that matters: it stops, it does not answer, and it says why.
#[tokio::test]
async fn unusable_responses_forever_stop_the_turn_instead_of_looping() {
    let mut h = harness(vec![empty(), empty(), empty(), empty(), empty(), empty()]);

    let outcome = h.session.run_turn("say something", None).await;

    match outcome {
        Ok(Outcome::Stopped(reason)) => assert!(!reason.is_empty(), "a stop must give a reason"),
        Ok(Outcome::Answer(a)) => panic!("an empty response was accepted as an answer: {a:?}"),
        Err(e) => panic!("expected a clean stop, got a provider error: {e}"),
    }
}

/// A malformed tool call is a bad *completion*, not a failed tool.
///
/// The loop does not dispatch it: `parse` returns `Malformed` with a precise
/// error, that error goes back to the model, and the model resends. Dispatching a
/// call whose arguments do not parse would replace "arguments are not valid JSON"
/// with whatever the tool made of the wreckage, which is strictly less useful.
///
/// So the requirements are: no tool runs, the model is told precisely what was
/// wrong, and the turn still recovers.
#[tokio::test]
async fn a_malformed_tool_call_is_fed_back_to_the_model_not_dispatched() {
    let mut h = harness(vec![
        tool_call("c1", "read", "{not json"),
        tool_call("c2", "read", r#"{"path": "notes.txt"}"#),
        answer("recovered"),
    ]);

    let (events, sink) = recording_sink();
    let outcome = h.session.run_turn("read it", Some(&sink)).await;

    assert!(matches!(outcome, Ok(Outcome::Answer(_))), "{outcome:?}");

    let log = events.lock().unwrap();
    assert!(
        !log.iter()
            .any(|e| matches!(e, TurnEvent::ToolStarted { id, .. } if id == "c1")),
        "the malformed call must not be dispatched"
    );
    assert!(
        log.iter().any(|e| matches!(e, TurnEvent::Warning(_))),
        "the retry has to be visible, or it looks like a silent stall: {log:?}"
    );

    let history = h.session.history().to_api().to_string();
    assert!(
        history.contains("not valid JSON"),
        "the model needs the precise parse error to resend correctly: {history}"
    );
    assert!(
        history.contains("alpha"),
        "and the corrected call still ran: {history}"
    );
}

/// A call to a tool that does not exist is the model's mistake to correct, and
/// the error names the tools that do exist.
#[tokio::test]
async fn an_unknown_tool_is_reported_with_the_available_ones() {
    let mut h = harness(vec![
        tool_call("c1", "no_such_tool", "{}"),
        answer("I will use a real one"),
    ]);

    h.session.run_turn("go", None).await.expect("turn runs");

    let history = h.session.history().to_api().to_string();
    assert!(history.contains("unknown tool"), "{history}");
    assert!(
        history.contains("read"),
        "the model needs to be told what it can call instead: {history}"
    );
}

// ---------------------------------------------------------------------------
// Gates
// ---------------------------------------------------------------------------

/// A hook denial reaches the model as a tool result, so it can choose something
/// else. Silently dropping it would leave the model waiting on an answer that
/// never comes.
#[tokio::test]
async fn a_denied_tool_tells_the_model_why_and_the_turn_continues() {
    let registry = Registry::core().with_hook(DenyTool("bash"));
    let mut h = harness_with(
        vec![
            tool_call("c1", "bash", r#"{"command": "ls"}"#),
            answer("I will not run that then"),
        ],
        registry,
        Limits::default(),
    );

    let outcome = h.session.run_turn("run ls", None).await;
    assert!(matches!(outcome, Ok(Outcome::Answer(_))));

    let history = h.session.history().to_api().to_string();
    assert!(
        history.contains("policy forbids this"),
        "the denial reason must reach the model: {history}"
    );
}

/// A denied write must not touch disk. This is the property the whole review
/// gate exists for, asserted here against a real filesystem.
#[tokio::test]
async fn a_denied_write_leaves_the_file_alone() {
    let registry = Registry::core().with_hook(DenyTool("write"));
    let mut h = harness_with(
        vec![
            tool_call(
                "c1",
                "write",
                r#"{"path": "notes.txt", "content": "REPLACED"}"#,
            ),
            answer("blocked"),
        ],
        registry,
        Limits::default(),
    );

    h.session
        .run_turn("overwrite it", None)
        .await
        .expect("turn");

    assert_eq!(
        h.ctx.workspace.read_to_string("notes.txt").expect("read"),
        "alpha\nbravo\n",
        "a denied write reached disk"
    );
}

// ---------------------------------------------------------------------------
// Ceilings
// ---------------------------------------------------------------------------

/// A model that only ever calls tools has to be stopped by the step ceiling
/// rather than running until something else gives out.
#[tokio::test]
async fn the_step_ceiling_stops_a_loop_that_never_answers() {
    let limits = Limits {
        max_steps: 3,
        ..Default::default()
    };

    let script = (0..20)
        .map(|i| tool_call(&format!("c{i}"), "read", r#"{"path": "notes.txt"}"#))
        .collect();
    let mut h = harness_with(script, Registry::core(), limits);

    let outcome = h.session.run_turn("keep reading", None).await;

    match outcome {
        Ok(Outcome::Stopped(reason)) => assert!(!reason.is_empty()),
        other => panic!("expected the step ceiling to stop the turn, got {other:?}"),
    }
    assert!(
        h.provider.call_count() <= 4,
        "the ceiling must bite promptly, not after the script runs out: {} calls",
        h.provider.call_count()
    );
}

/// The context ceiling is a latency guard, and it fires **before** the next
/// expensive call rather than after one that has already been paid for.
///
/// It is checked at the top of the loop, on the prompt size the endpoint actually
/// reported. So a turn that answers in one step is allowed to answer however large
/// its prompt was — there is nothing to save by stopping then — and a turn that
/// wants to continue is stopped before it can.
#[tokio::test]
async fn the_context_ceiling_stops_the_turn_before_the_next_call() {
    let limits = Limits {
        context_hard: 500,
        ..Default::default()
    };

    let mut h = harness_with(
        vec![
            // Answers in one step, but reports an enormous prompt.
            tool_call_at("c1", "read", r#"{"path": "notes.txt"}"#, 100_000),
            answer("this should never be reached"),
        ],
        Registry::core(),
        limits,
    );

    let outcome = h.session.run_turn("hello", None).await;

    match outcome {
        Ok(Outcome::Stopped(reason)) => assert!(!reason.is_empty(), "a stop must give a reason"),
        other => panic!("expected the context ceiling to stop the turn, got {other:?}"),
    }
    assert_eq!(
        h.provider.call_count(),
        1,
        "the ceiling exists to avoid the *next* prefill, so there must not be one"
    );
}

/// The negative control for the above: a large prompt on a turn that answers
/// immediately is not stopped, because stopping saves nothing.
#[tokio::test]
async fn a_large_prompt_that_answers_in_one_step_is_allowed_to_finish() {
    let limits = Limits {
        context_hard: 500,
        ..Default::default()
    };

    let mut h = harness_with(
        vec![answer_at("done despite the size", 100_000)],
        Registry::core(),
        limits,
    );

    let outcome = h.session.run_turn("hello", None).await;
    assert!(
        matches!(outcome, Ok(Outcome::Answer(_))),
        "there is nothing to save by refusing an answer already produced: {outcome:?}"
    );
}

// ---------------------------------------------------------------------------
// The invariant everything else rests on
// ---------------------------------------------------------------------------

/// History is append-only across turns, byte for byte.
///
/// There is already a unit test for this. It is worth repeating through the real
/// loop, with real tool dispatch, because that is where a stray mutation would
/// actually come from — and the cost of getting it wrong is a full cold prefill
/// on every subsequent turn, which at real context sizes is minutes.
#[tokio::test]
async fn nothing_already_in_history_changes_across_turns() {
    let mut h = harness(vec![
        tool_call("c1", "read", r#"{"path": "notes.txt"}"#),
        answer("first answer"),
        tool_call("c2", "ls", r#"{"path": "."}"#),
        answer("second answer"),
    ]);

    h.session.run_turn("first", None).await.expect("turn one");
    let after_first: Vec<String> = h
        .session
        .history()
        .messages()
        .iter()
        .map(|m| serde_json::to_string(m).expect("serialisable"))
        .collect();

    h.session.run_turn("second", None).await.expect("turn two");
    let after_second: Vec<String> = h
        .session
        .history()
        .messages()
        .iter()
        .map(|m| serde_json::to_string(m).expect("serialisable"))
        .collect();

    assert!(
        after_second.len() > after_first.len(),
        "the second turn appended nothing"
    );
    for (i, before) in after_first.iter().enumerate() {
        assert_eq!(
            before, &after_second[i],
            "message {i} changed between turns — the cached prefix is invalidated"
        );
    }
}

/// The system prompt is the first thing in the cached prefix. If it moves, every
/// turn pays a cold prefill.
#[tokio::test]
async fn the_system_prompt_stays_put_across_turns() {
    let mut h = harness(vec![answer("one"), answer("two")]);

    h.session.run_turn("a", None).await.expect("turn one");
    let first = h.session.history().system_prompt().map(str::to_owned);
    h.session.run_turn("b", None).await.expect("turn two");

    assert_eq!(first.as_deref(), h.session.history().system_prompt());
    assert_eq!(
        h.session.history().messages()[0].role,
        smithy_agent::Role::System,
        "the system message must remain first"
    );
}
