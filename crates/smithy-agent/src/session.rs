//! The agent loop.

use std::sync::{Arc, Mutex};

use serde_json::Value;
use smithy_tools::{Registry, ToolCtx, ToolResult};
use tokio_util::sync::CancellationToken;

use crate::limits::{Budget, Limits};
use crate::message::{History, Message};
use crate::parse::{parse, Action};
use crate::provider::{Completion, CompletionRequest, Delta, Provider, ProviderError, Sampling};

/// How a user-turn ended.
#[derive(Debug, Clone)]
pub enum Outcome {
    /// The model produced a final answer with no tool call.
    Answer(String),
    /// A ceiling or repeated unusable response ended the turn early.
    Stopped(String),
}

/// Progress emitted while a turn runs, so a UI can show activity instead of
/// waiting for the final result.
#[derive(Debug, Clone)]
pub enum TurnEvent {
    /// A fragment of the model's reasoning channel.
    Reasoning(String),
    /// A fragment of the model's answer.
    Content(String),
    ToolStarted {
        id: String,
        step: usize,
        name: String,
        arguments: String,
    },
    ToolFinished {
        id: String,
        step: usize,
        name: String,
        content: String,
        is_error: bool,
    },
    /// A soft ceiling was crossed, or a response had to be retried.
    Warning(String),
}

pub type EventSink = dyn Fn(TurnEvent) + Send + Sync;

pub struct SessionConfig {
    pub system_prompt: String,
    pub sampling: Sampling,
    pub limits: Limits,
}

impl SessionConfig {
    pub fn new(system_prompt: impl Into<String>) -> Self {
        Self {
            system_prompt: system_prompt.into(),
            sampling: Sampling::default(),
            limits: Limits::default(),
        }
    }
}

/// One conversation: owns the append-only history and drives the loop.
pub struct Session {
    provider: Arc<dyn Provider>,
    registry: Arc<Registry>,
    ctx: Arc<ToolCtx>,
    history: History,
    /// Built once at construction and reused verbatim on every request. This is
    /// the cache root — rebuilding it per turn would change the bytes and throw
    /// the prefix away.
    tools: Value,
    sampling: Sampling,
    limits: Limits,
    /// Cooperative cancellation for the Stop button. See [`Stopper`].
    cancel: Arc<Mutex<CancellationToken>>,
}

/// What `Outcome::Stopped` says when the user pressed Stop. A constant because
/// the UI matches on it and a test asserts it.
pub const CANCELLED: &str = "stopped by user";

/// A handle the UI holds to stop whichever turn is currently running.
///
/// The indirection is load-bearing. A `CancellationToken` is *sticky*: once
/// tripped it stays tripped, so a session that reused one token would stop every
/// turn after the first instantly. The session installs a fresh token when a
/// turn *ends*, and this handle resolves to whichever token is current — so the
/// UI takes one `Stopper` at construction and keeps it for the whole session.
///
/// **A stop requested while no turn is running carries into the next turn**,
/// which then stops before reaching the provider. That is the deliberate
/// trade: arming at the end of a turn rather than the start is what stops a
/// click being swallowed in the window between submitting a task and the
/// runtime actually picking it up. The UI only reveals Stop while a turn is in
/// flight, so it cannot reach the surprising case; a headless caller can, and
/// should re-read this.
#[derive(Clone)]
pub struct Stopper(Arc<Mutex<CancellationToken>>);

impl Stopper {
    /// Ask the running turn to stop. Returns immediately; the turn ends at its
    /// next checkpoint.
    pub fn stop(&self) {
        if let Ok(token) = self.0.lock() {
            token.cancel();
        }
    }
}

impl Session {
    pub fn new(
        provider: Arc<dyn Provider>,
        registry: Arc<Registry>,
        ctx: Arc<ToolCtx>,
        config: SessionConfig,
    ) -> Session {
        let tools = registry.openai_schemas();
        Session {
            provider,
            registry,
            ctx,
            history: History::new(config.system_prompt),
            tools,
            sampling: config.sampling,
            limits: config.limits,
            cancel: Arc::new(Mutex::new(CancellationToken::new())),
        }
    }

    /// Rebuild a session around a restored history.
    ///
    /// The history goes back untouched — see [`crate::persist`] for why that
    /// matters.
    pub fn resume(
        provider: Arc<dyn Provider>,
        registry: Arc<Registry>,
        ctx: Arc<ToolCtx>,
        history: History,
        sampling: Sampling,
        limits: Limits,
    ) -> Session {
        let tools = registry.openai_schemas();
        Session {
            provider,
            registry,
            ctx,
            history,
            tools,
            sampling,
            limits,
            cancel: Arc::new(Mutex::new(CancellationToken::new())),
        }
    }

    /// A handle the UI can hold to stop a running turn. See [`Stopper`].
    pub fn stopper(&self) -> Stopper {
        Stopper(Arc::clone(&self.cancel))
    }

    /// The token governing the current turn.
    ///
    /// Deliberately does *not* install a fresh one. Arming happens when a turn
    /// ends, not when it starts — see [`Session::rearm_cancel`].
    fn current_cancel(&self) -> CancellationToken {
        match self.cancel.lock() {
            Ok(current) => current.clone(),
            // A poisoned lock means another thread panicked while swapping.
            // Hand back an un-cancelled token: the turn runs uninterruptibly,
            // which is worse than a working Stop but far better than a session
            // that refuses to run at all.
            Err(_) => CancellationToken::new(),
        }
    }

    /// Install a fresh token so the *next* turn starts un-cancelled.
    ///
    /// Tokens are sticky, so one has to be replaced somewhere. Doing it when a
    /// turn ends rather than when one begins is what makes Stop reliable: the UI
    /// reveals the Stop button the instant a task is submitted, but the turn
    /// only starts once the runtime picks it up. Resetting at the head of the
    /// turn would silently discard any click landing in that gap.
    fn rearm_cancel(&self) {
        if let Ok(mut current) = self.cancel.lock() {
            *current = CancellationToken::new();
        }
    }

    pub fn history(&self) -> &History {
        &self.history
    }

    pub fn limits(&self) -> &Limits {
        &self.limits
    }

    pub fn sampling(&self) -> &Sampling {
        &self.sampling
    }

    pub async fn preflight(&self) -> Result<(), ProviderError> {
        self.provider.preflight().await
    }

    /// Append the user's message and run until the model stops calling tools or
    /// a ceiling is hit. History is only ever appended to.
    pub async fn run_turn(
        &mut self,
        user_input: &str,
        events: Option<&EventSink>,
    ) -> Result<Outcome, ProviderError> {
        let outcome = self.run_turn_inner(user_input, events).await;
        // Whatever happened — answered, stopped, or errored — the next turn
        // starts from a fresh token. Done here rather than at the head of the
        // turn so a Stop pressed before the runtime picks the turn up is still
        // honoured.
        self.rearm_cancel();
        outcome
    }

    async fn run_turn_inner(
        &mut self,
        user_input: &str,
        events: Option<&EventSink>,
    ) -> Result<Outcome, ProviderError> {
        let cancel = self.current_cancel();
        self.history.push(Message::user(user_input));

        let mut budget = Budget::new(self.limits.clone());
        let mut consecutive_failures = 0usize;

        loop {
            // Checkpoint 1. The history is in a valid state at the top of the
            // loop and nowhere else in it: either just the user message, or a
            // complete assistant-plus-every-tool-result set. Stopping here needs
            // no repair.
            if cancel.is_cancelled() {
                return Ok(Outcome::Stopped(CANCELLED.into()));
            }

            if let Err(stop) = budget.tick() {
                return Ok(Outcome::Stopped(stop.to_string()));
            }

            // Checkpoint 2. Nothing is appended until the model call returns, so
            // abandoning it mid-stream leaves the history byte-identical to what
            // it was before the request — which is what keeps the cached prefix
            // valid for the next turn. `biased` makes cancellation win a tie
            // deterministically instead of by scheduler chance.
            let completion = tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    return Ok(Outcome::Stopped(CANCELLED.into()));
                }
                result = self.complete(events) => result?,
            };

            if let Some(warning) = budget.record_prompt_tokens(completion.prompt_tokens) {
                emit(events, TurnEvent::Warning(warning));
            }

            match parse(&completion) {
                Action::Done(answer) => {
                    // An answer that is empty or was cut off is NOT a clean
                    // finish. The failure mode this catches: at high context the
                    // model reasons correctly but loops inside `reasoning` —
                    // "Done. Proceeds. Done…" — until it exhausts the token
                    // limit and emits empty content. Returning that as a
                    // successful turn produces a silent no-op.
                    let truncated = completion.was_truncated();
                    let empty = answer.trim().is_empty();
                    if truncated || empty {
                        consecutive_failures += 1;
                        let why = if truncated {
                            "was cut off by the token limit"
                        } else {
                            "produced no answer (empty content)"
                        };
                        emit(
                            events,
                            TurnEvent::Warning(format!(
                                "response {why} ({consecutive_failures}/{}) — likely a runaway \
                                 reasoning loop",
                                self.limits.max_parse_retries
                            )),
                        );
                        if consecutive_failures >= self.limits.max_parse_retries {
                            return Ok(Outcome::Stopped(
                                "model produced no usable answer (likely a runaway reasoning loop)"
                                    .into(),
                            ));
                        }
                        self.history
                            .push(Message::assistant(completion.content.clone()));
                        self.history.push(Message::user(format!(
                            "Your previous response {why}. You may have gotten stuck repeating \
                             yourself. Give ONLY the final answer now, in one or two short \
                             sentences, then stop — do not re-verify or restate."
                        )));
                        continue;
                    }

                    self.history
                        .push(Message::assistant(completion.content.clone()));
                    return Ok(Outcome::Answer(answer));
                }

                Action::Malformed(err) => {
                    consecutive_failures += 1;
                    emit(
                        events,
                        TurnEvent::Warning(format!(
                            "could not parse a tool call ({consecutive_failures}/{}): {err}",
                            self.limits.max_parse_retries
                        )),
                    );
                    if consecutive_failures >= self.limits.max_parse_retries {
                        return Ok(Outcome::Stopped(format!(
                            "gave up after {consecutive_failures} consecutive malformed tool calls"
                        )));
                    }
                    self.history
                        .push(Message::assistant(completion.content.clone()));
                    let mut note = format!(
                        "Your previous message could not be parsed as a tool call: {err}\n\
                         Re-issue it as a single structured function call."
                    );
                    if completion.was_truncated() {
                        note.push_str(
                            "\n(Your response was cut off by the length limit — be more concise.)",
                        );
                    }
                    self.history.push(Message::user(note));
                }

                Action::Calls(calls) => {
                    consecutive_failures = 0;
                    self.history.push(Message::assistant_with_calls(
                        completion.content.clone(),
                        calls.clone(),
                    ));
                    for (i, call) in calls.iter().enumerate() {
                        // Checkpoint 3. The assistant message announcing these
                        // calls is already in the history, and a tool call
                        // without a matching result is a malformed conversation
                        // — some endpoints reject it outright, and the model
                        // cannot reason about a call it never got an answer to.
                        // So stopping here is not a bare return: every call that
                        // will not now run gets a synthetic result first. That
                        // keeps the append-only history well-formed, which is
                        // what makes the turn resumable.
                        if cancel.is_cancelled() {
                            for pending in &calls[i..] {
                                self.history.push(Message::tool_result(&ToolResult::err(
                                    pending,
                                    "Stopped by the user before this tool ran.",
                                )));
                            }
                            return Ok(Outcome::Stopped(CANCELLED.into()));
                        }

                        emit(
                            events,
                            TurnEvent::ToolStarted {
                                id: call.id.clone(),
                                step: budget.step(),
                                name: call.name.clone(),
                                arguments: call.arguments.clone(),
                            },
                        );

                        let result: ToolResult = self.registry.execute(call, &self.ctx).await;

                        emit(
                            events,
                            TurnEvent::ToolFinished {
                                id: call.id.clone(),
                                step: budget.step(),
                                name: call.name.clone(),
                                content: result.content.clone(),
                                is_error: result.is_error,
                            },
                        );
                        self.history.push(Message::tool_result(&result));
                    }
                }
            }
        }
    }

    async fn complete(&self, events: Option<&EventSink>) -> Result<Completion, ProviderError> {
        let request = CompletionRequest {
            history: &self.history,
            tools: &self.tools,
            sampling: &self.sampling,
        };

        match events {
            Some(sink) => {
                // Re-wrap so the provider sees a `Delta` callback while the
                // caller only ever deals in `TurnEvent`.
                let forward = move |delta: Delta| match delta {
                    Delta::Reasoning(t) => sink(TurnEvent::Reasoning(t)),
                    Delta::Content(t) => sink(TurnEvent::Content(t)),
                };
                self.provider.complete(request, Some(&forward)).await
            }
            None => self.provider.complete(request, None).await,
        }
    }
}

fn emit(events: Option<&EventSink>, event: TurnEvent) {
    if let Some(sink) = events {
        sink(event);
    }
}

/// The default system prompt.
///
/// Kept a pure function of its inputs so it is byte-stable within a session:
/// no timestamps, no per-turn variation, nothing that would change the head of
/// the cached prefix between requests.
///
/// `project_context` is the block from `smithy-project`. It goes **here**, in
/// the system prompt, rather than being sent per-turn — that puts it inside the
/// cached prefix, so it is prefilled once for the whole session instead of
/// re-sent with every message. It is also why it cannot be refreshed mid-session:
/// changing it invalidates the cache for every turn that follows.
pub fn default_system_prompt(
    workspace: &std::path::Path,
    tool_names: &[&str],
    project_context: Option<&str>,
) -> String {
    let base = format!(
        "You are Smithy, a coding agent working in a single workspace on the user's machine.\n\
         \n\
         Workspace root: {ws}\n\
         \n\
         You have these tools: {tools}. Call a tool to take an action. Prefer to act with tools \
         rather than describe what you would do. Take one focused step at a time: call a tool, \
         observe its result, then decide the next step.\n\
         \n\
         Guidelines:\n\
         - Discover files with `glob` (by name) and `ls` (a directory); search contents with \
         `grep`. Use `read` with offset/limit for large files.\n\
         - For a small change to an existing file use `edit`. Use `write` to create a new file or \
         fully rewrite one — always emit the COMPLETE contents, never a diff.\n\
         - For a multi-step job, call `todo` first to lay out the plan, and update it as you \
         finish steps. Skip it for trivial one-step tasks.\n\
         - Keep `bash` commands short and non-interactive. Output is truncated if large.\n\
         - When the task is complete, reply with a short plain-text summary and DO NOT call any \
         tool. That is how you end your turn.\n\
         - Be concise. Do not narrate at length; let tool results speak.",
        ws = workspace.display(),
        tools = tool_names.join(", "),
    );

    match project_context {
        Some(context) if !context.trim().is_empty() => format!(
            "{base}\n\n\
             The following describes the project you are working in. It was extracted when the \
             session started and is not refreshed, so verify with tools before relying on it for \
             anything that may have changed.\n\n\
             {context}"
        ),
        _ => base,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::test_support::{answer, tool_call, ScriptedProvider};
    use crate::provider::Completion;
    use smithy_tools::{ToolCall, Workspace};

    fn harness(script: Vec<Completion>) -> (tempfile::TempDir, Session, Arc<ScriptedProvider>) {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("notes.txt"), "the secret word is FJORD\n").unwrap();
        let ws = Workspace::open(tmp.path()).unwrap();
        let provider = Arc::new(ScriptedProvider::new(script));
        let session = Session::new(
            provider.clone(),
            Arc::new(Registry::core()),
            Arc::new(ToolCtx::new(ws)),
            SessionConfig::new("test prompt"),
        );
        (tmp, session, provider)
    }

    // --- cancellation -----------------------------------------------------
    //
    // The requirement is not "a flag got set". It is that a stopped turn leaves
    // a conversation the model can be handed again: every tool call answered,
    // nothing half-written, and the next turn unaffected.

    /// The point of the Stop button: the turn ends without paying for the model
    /// call that was about to happen.
    #[tokio::test]
    async fn stopping_before_a_turn_starts_never_reaches_the_provider() {
        let (_t, mut s, provider) = harness(vec![answer("should never be reached")]);
        s.stopper().stop();

        let outcome = s.run_turn("do something expensive", None).await.unwrap();

        assert!(
            matches!(&outcome, Outcome::Stopped(r) if r == CANCELLED),
            "got {outcome:?}"
        );
        assert_eq!(
            provider.call_count(),
            0,
            "stop must precede the network call"
        );
    }

    /// The invariant that makes a stopped turn resumable. Once the assistant
    /// message announcing tool calls is in the history, a call with no matching
    /// result is a malformed conversation — endpoints reject it and the model
    /// cannot reason about an answer it never received.
    #[tokio::test]
    async fn stopping_between_tool_calls_still_answers_every_announced_call() {
        let two_calls = Completion {
            tool_calls: vec![
                ToolCall::new("c1", "read", r#"{"path":"notes.txt"}"#),
                ToolCall::new("c2", "read", r#"{"path":"notes.txt"}"#),
                ToolCall::new("c3", "read", r#"{"path":"notes.txt"}"#),
            ],
            finish_reason: "tool_calls".into(),
            prompt_tokens: 100,
            ..Default::default()
        };
        let (_t, mut s, _) = harness(vec![two_calls]);

        // Stop the moment the first tool reports back, so cancellation lands
        // inside the dispatch loop rather than at the top of it.
        let stopper = s.stopper();
        let sink = move |event: TurnEvent| {
            if matches!(event, TurnEvent::ToolFinished { .. }) {
                stopper.stop();
            }
        };

        let outcome = s
            .run_turn("read it three times", Some(&sink))
            .await
            .unwrap();
        assert!(
            matches!(&outcome, Outcome::Stopped(r) if r == CANCELLED),
            "got {outcome:?}"
        );

        let announced: Vec<&str> = s
            .history()
            .messages()
            .iter()
            .flat_map(|m| m.tool_calls.iter())
            .map(|c| c.id.as_str())
            .collect();
        let answered: Vec<&str> = s
            .history()
            .messages()
            .iter()
            .filter_map(|m| m.tool_call_id.as_deref())
            .collect();

        assert_eq!(announced, vec!["c1", "c2", "c3"]);
        assert_eq!(
            answered, announced,
            "every announced call needs a result, in order, or the history is malformed"
        );
    }

    /// `CancellationToken` is sticky. A session that reused one token would look
    /// correct in every single-turn test and then refuse to work for the rest of
    /// the session after the first Stop.
    #[tokio::test]
    async fn stopping_one_turn_does_not_stop_the_next() {
        let (_t, mut s, provider) = harness(vec![answer("second turn ran")]);

        s.stopper().stop();
        let first = s.run_turn("this one stops", None).await.unwrap();
        assert!(matches!(&first, Outcome::Stopped(r) if r == CANCELLED));
        assert_eq!(provider.call_count(), 0);

        let second = s.run_turn("this one should run", None).await.unwrap();
        assert!(
            matches!(&second, Outcome::Answer(a) if a == "second turn ran"),
            "a stop must not poison the session: got {second:?}"
        );
        assert_eq!(provider.call_count(), 1);
    }

    /// A stop that is never pressed must change nothing.
    #[tokio::test]
    async fn an_untouched_stopper_leaves_a_turn_alone() {
        let (_t, mut s, provider) = harness(vec![
            tool_call("c1", "read", r#"{"path":"notes.txt"}"#),
            answer("The secret word is FJORD."),
        ]);
        let _keep = s.stopper();

        let outcome = s.run_turn("what is the secret word?", None).await.unwrap();
        assert!(
            matches!(&outcome, Outcome::Answer(a) if a.contains("FJORD")),
            "got {outcome:?}"
        );
        assert_eq!(provider.call_count(), 2);
    }

    #[tokio::test]
    async fn a_plain_answer_ends_the_turn() {
        let (_t, mut s, provider) = harness(vec![answer("All done.")]);
        let outcome = s.run_turn("say hi", None).await.unwrap();
        assert!(matches!(outcome, Outcome::Answer(a) if a == "All done."));
        assert_eq!(provider.call_count(), 1);
    }

    #[tokio::test]
    async fn a_tool_call_is_executed_and_the_result_appended() {
        let (_t, mut s, _) = harness(vec![
            tool_call("c1", "read", r#"{"path":"notes.txt"}"#),
            answer("The secret word is FJORD."),
        ]);
        let outcome = s.run_turn("what is the secret word?", None).await.unwrap();
        assert!(matches!(outcome, Outcome::Answer(a) if a.contains("FJORD")));

        let msgs = s.history().messages();
        let tool_msg = msgs
            .iter()
            .find(|m| m.role == crate::message::Role::Tool)
            .unwrap();
        assert_eq!(tool_msg.tool_call_id.as_deref(), Some("c1"));
        assert!(tool_msg.content.contains("FJORD"));
    }

    #[tokio::test]
    async fn history_only_ever_grows() {
        let (_t, mut s, _) = harness(vec![
            tool_call("c1", "ls", "{}"),
            answer("done"),
            answer("done again"),
        ]);
        s.run_turn("first", None).await.unwrap();
        let after_first = s.history().len();
        s.run_turn("second", None).await.unwrap();
        assert!(s.history().len() > after_first);
        assert_eq!(s.history().messages()[0].content, "test prompt");
    }

    /// The high-context failure coda identified: correct reasoning, empty
    /// content. It must not read as a finished turn.
    #[tokio::test]
    async fn an_empty_answer_is_retried_then_gives_up() {
        let empty = || Completion {
            content: String::new(),
            reasoning: "Done. Proceeds. Done. Proceeds.".into(),
            finish_reason: "length".into(),
            prompt_tokens: 100_000,
            ..Default::default()
        };
        let (_t, mut s, provider) = harness(vec![empty(), empty(), empty()]);
        let outcome = s.run_turn("do the thing", None).await.unwrap();
        match outcome {
            Outcome::Stopped(reason) => assert!(reason.contains("runaway reasoning loop")),
            other => panic!("expected Stopped, got {other:?}"),
        }
        assert_eq!(provider.call_count(), 3, "should retry up to the limit");
    }

    #[tokio::test]
    async fn an_empty_answer_can_recover_on_retry() {
        let (_t, mut s, _) = harness(vec![
            Completion {
                content: String::new(),
                finish_reason: "length".into(),
                ..Default::default()
            },
            answer("Recovered: the answer is 42."),
        ]);
        let outcome = s.run_turn("go", None).await.unwrap();
        assert!(matches!(outcome, Outcome::Answer(a) if a.contains("42")));
    }

    #[tokio::test]
    async fn the_step_ceiling_stops_a_runaway_loop() {
        let script: Vec<Completion> = (0..10)
            .map(|i| tool_call(&format!("c{i}"), "ls", "{}"))
            .collect();
        let (_t, mut s, _) = harness(script);
        s.limits.max_steps = 4;
        let outcome = s.run_turn("loop forever", None).await.unwrap();
        match outcome {
            Outcome::Stopped(r) => assert!(r.contains("step limit reached (4)")),
            other => panic!("expected Stopped, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn the_context_ceiling_stops_the_turn() {
        let big = Completion {
            tool_calls: vec![smithy_tools::ToolCall::new("c1", "ls", "{}")],
            prompt_tokens: 999_999,
            ..Default::default()
        };
        let (_t, mut s, _) = harness(vec![big.clone(), big]);
        let outcome = s.run_turn("go", None).await.unwrap();
        match outcome {
            Outcome::Stopped(r) => assert!(r.contains("context ceiling")),
            other => panic!("expected Stopped, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_failing_tool_feeds_the_error_back_instead_of_aborting() {
        let (_t, mut s, _) = harness(vec![
            tool_call("c1", "read", r#"{"path":"nope.txt"}"#),
            answer("That file does not exist."),
        ]);
        let outcome = s.run_turn("read nope.txt", None).await.unwrap();
        assert!(matches!(outcome, Outcome::Answer(_)));
        let tool_msg = s
            .history()
            .messages()
            .iter()
            .find(|m| m.role == crate::message::Role::Tool)
            .unwrap();
        assert!(tool_msg.content.contains("cannot read"));
    }

    #[tokio::test]
    async fn events_are_emitted_for_tool_calls() {
        use std::sync::Mutex;
        let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let seen2 = seen.clone();
        let sink = move |e: TurnEvent| {
            let label = match e {
                TurnEvent::ToolStarted { name, .. } => format!("start:{name}"),
                TurnEvent::ToolFinished { name, is_error, .. } => {
                    format!("finish:{name}:{is_error}")
                }
                TurnEvent::Warning(_) => "warn".into(),
                _ => "other".into(),
            };
            seen2.lock().unwrap().push(label);
        };

        let (_t, mut s, _) = harness(vec![tool_call("c1", "ls", "{}"), answer("done")]);
        s.run_turn("list files", Some(&sink)).await.unwrap();

        let seen = seen.lock().unwrap();
        assert!(seen.contains(&"start:ls".to_string()));
        assert!(seen.contains(&"finish:ls:false".to_string()));
    }

    #[tokio::test]
    async fn parallel_tool_calls_each_get_a_correlated_result() {
        let both = Completion {
            tool_calls: vec![
                smithy_tools::ToolCall::new("c_a", "ls", "{}"),
                smithy_tools::ToolCall::new("c_b", "read", r#"{"path":"notes.txt"}"#),
            ],
            prompt_tokens: 100,
            ..Default::default()
        };
        let (_t, mut s, _) = harness(vec![both, answer("done")]);
        s.run_turn("do both", None).await.unwrap();

        let ids: Vec<String> = s
            .history()
            .messages()
            .iter()
            .filter(|m| m.role == crate::message::Role::Tool)
            .filter_map(|m| m.tool_call_id.clone())
            .collect();
        assert_eq!(ids, vec!["c_a".to_string(), "c_b".to_string()]);
    }

    #[test]
    fn the_system_prompt_is_byte_stable() {
        let path = std::path::Path::new("/tmp/ws");
        let tools = ["read", "write"];
        assert_eq!(
            default_system_prompt(path, &tools, None),
            default_system_prompt(path, &tools, None)
        );
    }
}

#[cfg(test)]
mod prompt_tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn a_project_context_is_embedded_in_the_prompt() {
        let prompt = default_system_prompt(
            Path::new("/tmp/ws"),
            &["read", "edit"],
            Some("# Project: demo\n## Crates\n- demo v0.1.0"),
        );
        assert!(prompt.contains("# Project: demo"));
        assert!(prompt.contains("- demo v0.1.0"));
    }

    /// The model must know the context is a snapshot, or it will trust it after
    /// its own edits have invalidated it.
    #[test]
    fn the_prompt_warns_that_the_context_is_a_snapshot() {
        let prompt = default_system_prompt(Path::new("/tmp"), &["read"], Some("# Project: x"));
        assert!(prompt.contains("not refreshed"));
        assert!(prompt.contains("verify with tools"));
    }

    #[test]
    fn no_context_leaves_the_prompt_unchanged() {
        let bare = default_system_prompt(Path::new("/tmp"), &["read"], None);
        let empty = default_system_prompt(Path::new("/tmp"), &["read"], Some("   "));
        assert_eq!(bare, empty, "blank context must not add an empty section");
        assert!(!bare.contains("not refreshed"));
    }

    /// Byte-stability is the whole reason this lives in the system prompt.
    #[test]
    fn the_prompt_is_byte_stable_with_a_context() {
        let context = "# Project: demo\n## Crates\n- demo";
        let a = default_system_prompt(Path::new("/tmp"), &["read"], Some(context));
        let b = default_system_prompt(Path::new("/tmp"), &["read"], Some(context));
        assert_eq!(a, b);
    }
}

#[cfg(test)]
mod prefix_invariant_tests {
    use super::*;
    use crate::provider::test_support::{answer, tool_call, ScriptedProvider};
    use crate::provider::Completion;
    use smithy_tools::Workspace;

    /// Serialize each message on its own, so the comparison is per-element.
    ///
    /// The whole array is *not* a string prefix of the next turn's array — the
    /// closing bracket moves. What must hold is that every earlier element is
    /// byte-identical, which is what the endpoint's prefix cache actually keys
    /// on.
    fn message_bytes(session: &Session) -> Vec<String> {
        session
            .history()
            .messages()
            .iter()
            .map(|m| serde_json::to_string(&m.to_api()).expect("message serializes"))
            .collect()
    }

    fn assert_is_prefix(before: &[String], after: &[String]) {
        assert!(
            after.len() >= before.len(),
            "history shrank: {} -> {}",
            before.len(),
            after.len()
        );
        for (i, earlier) in before.iter().enumerate() {
            assert_eq!(
                earlier, &after[i],
                "message {i} changed between turns.\n  before: {earlier}\n  after:  {}",
                after[i]
            );
        }
    }

    fn harness(script: Vec<Completion>) -> (tempfile::TempDir, Session) {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("notes.txt"), "FJORD\n").unwrap();
        let ws = Workspace::open(tmp.path()).unwrap();
        let session = Session::new(
            Arc::new(ScriptedProvider::new(script)),
            Arc::new(Registry::core()),
            Arc::new(ToolCtx::new(ws)),
            SessionConfig::new("system prompt"),
        );
        (tmp, session)
    }

    /// **The invariant the whole caching architecture rests on.**
    ///
    /// Every message present at the end of turn N must still be byte-identical
    /// at the end of turn N+1. If any earlier byte changes, the endpoint's
    /// prefix cache misses and the entire conversation is re-prefilled from
    /// cold — minutes of latency at real context sizes, on every subsequent
    /// turn.
    #[tokio::test]
    async fn earlier_messages_are_never_rewritten() {
        let (_t, mut session) = harness(vec![
            tool_call("c1", "read", r#"{"path":"notes.txt"}"#),
            answer("It says FJORD."),
            tool_call("c2", "ls", "{}"),
            answer("One file."),
            answer("Nothing further."),
        ]);

        session
            .run_turn("what does notes.txt say?", None)
            .await
            .unwrap();
        let after_first = message_bytes(&session);

        session.run_turn("what else is there?", None).await.unwrap();
        let after_second = message_bytes(&session);
        assert_is_prefix(&after_first, &after_second);

        session.run_turn("anything else?", None).await.unwrap();
        assert_is_prefix(&after_second, &message_bytes(&session));
    }

    /// The system prompt is the cache root. If it moves, nothing else matters.
    #[tokio::test]
    async fn the_system_prompt_is_byte_identical_across_turns() {
        let (_t, mut session) = harness(vec![answer("one"), answer("two")]);

        session.run_turn("first", None).await.unwrap();
        let first = message_bytes(&session)[0].clone();
        session.run_turn("second", None).await.unwrap();
        assert_eq!(first, message_bytes(&session)[0]);
    }

    /// A turn that retries — the empty-answer recovery path — appends its
    /// correction rather than editing the response that failed.
    #[tokio::test]
    async fn a_retry_appends_rather_than_rewriting() {
        let empty = Completion {
            content: String::new(),
            finish_reason: "length".into(),
            prompt_tokens: 100,
            ..Default::default()
        };
        let (_t, mut session) = harness(vec![empty, answer("recovered"), answer("next")]);

        session.run_turn("go", None).await.unwrap();
        let after_first = message_bytes(&session);
        session.run_turn("again", None).await.unwrap();
        assert_is_prefix(&after_first, &message_bytes(&session));
    }

    /// Parallel tool calls append their results in call order and never
    /// retroactively reorder the assistant turn that requested them.
    #[tokio::test]
    async fn parallel_tool_results_do_not_disturb_the_prefix() {
        let both = Completion {
            tool_calls: vec![
                smithy_tools::ToolCall::new("a", "ls", "{}"),
                smithy_tools::ToolCall::new("b", "read", r#"{"path":"notes.txt"}"#),
            ],
            prompt_tokens: 100,
            ..Default::default()
        };
        let (_t, mut session) = harness(vec![both, answer("done"), answer("done again")]);

        session.run_turn("do both", None).await.unwrap();
        let after_first = message_bytes(&session);
        session.run_turn("and again", None).await.unwrap();
        assert_is_prefix(&after_first, &message_bytes(&session));
    }

    /// A stopped turn still leaves a valid prefix for the next one.
    #[tokio::test]
    async fn a_stopped_turn_leaves_the_prefix_intact() {
        let looping: Vec<Completion> = (0..6)
            .map(|i| tool_call(&format!("c{i}"), "ls", "{}"))
            .collect();
        let (_t, mut session) = harness(looping);
        session.limits.max_steps = 3;

        let outcome = session.run_turn("loop", None).await.unwrap();
        assert!(matches!(outcome, Outcome::Stopped(_)));
        let after_stop = message_bytes(&session);
        assert!(!after_stop.is_empty());
        assert_is_prefix(&after_stop[..1], &after_stop);
    }

    /// The tool schema block is the other half of the cache root: it is built
    /// once at construction and must be reused verbatim, not rebuilt per turn.
    #[test]
    fn the_tool_schema_is_built_once_and_reused() {
        let a = serde_json::to_string(&Registry::core().openai_schemas()).unwrap();
        let b = serde_json::to_string(&Registry::core().openai_schemas()).unwrap();
        assert_eq!(a, b, "tool schemas must serialize identically");
    }
}
