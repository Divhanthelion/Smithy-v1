//! `explore` — a read-only research sub-agent.
//!
//! ## Why this is narrow, and why it is here at all
//!
//! The published evidence on multi-agent systems is not the encouragement it is
//! usually quoted as. Anthropic's own write-up of their research system reports
//! a large win on breadth-first research *and* says plainly that most coding
//! tasks are a poor fit, because they need shared context and have fewer truly
//! parallel parts than research does. It also puts the cost at roughly fifteen
//! times a chat interaction, and finds that token usage alone explains about
//! eighty per cent of the variance in outcome.
//!
//! So this is not an orchestrator, and it does not decompose your task. It does
//! the one sub-task that genuinely has the shape multi-agent is good at:
//! **answering a bounded question about a large corpus, and returning a small
//! answer**. Twenty greps and forty file reads happen inside this tool's own
//! history and die there; what comes back is a paragraph and some line numbers.
//! That is the entire point — on a local endpoint, where prefill dominates,
//! keeping forty file reads out of the main conversation is worth more than the
//! parallelism ever would be.
//!
//! ## Why it lives in `smithy-agent` and not `smithy-tools`
//!
//! [`smithy_tools::ToolCtx`] documents that a tool gets the workspace and the
//! todo list "and nothing else. It cannot reach the model, the UI, or the
//! filesystem outside the workspace." That is a real invariant and this tool
//! would break it — it needs a [`Provider`]. Since `smithy-agent` already
//! depends on `smithy-tools`, implementing [`Tool`] on this side of the boundary
//! keeps the invariant exactly as written: no tool *in the tools crate* can
//! reach a model, and the one that can is assembled by the app, deliberately.
//!
//! ## The three things that keep it from being a liability
//!
//! - **A restricted registry.** Read, search, list, and fetch. No `write`, no
//!   `edit`, no `bash`. A research agent that can edit files is not a research
//!   agent, and the write-review hook is not installed on it — so a write here
//!   would bypass the review gate entirely rather than merely being surprising.
//! - **No `explore` inside `explore`.** The sub-registry cannot contain this
//!   tool, or a model with a hard question recurses until something runs out.
//! - **A hard step ceiling, stated in the description.** The lesson from the
//!   same write-up: agents given explicit effort budgets ("simple fact-finding:
//!   one agent, 3-10 tool calls") stop over-investing in simple questions. The
//!   budget is enforced by [`Limits`] and *also* written into the tool
//!   description, because only one of those two stops the call being made.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;
use smithy_tools::registry::{Tool, ToolCtx};
use smithy_tools::schema::{arg_str, arg_str_opt, ToolDefinition, ToolOutput, ToolParameter};
use smithy_tools::{ExecutionControl, Registry, Workspace};

use crate::limits::Limits;
use crate::provider::{Provider, Sampling};
use crate::session::{Outcome, Session, SessionConfig};

/// How many tool calls one exploration may make.
///
/// Twelve, not sixty. A question that cannot be answered in twelve reads is a
/// question that should come back partially answered and say so, rather than
/// silently spending a minute of prefill on the fortieth grep.
pub const MAX_STEPS: usize = 12;

/// Wall-clock ceiling for one exploration.
pub const MAX_SECONDS: u64 = 180;

/// Context ceiling for the *sub*-session.
///
/// Much lower than the main session's. The sub-agent reads a lot and is supposed
/// to summarise, so a sub-agent approaching the main ceiling has already failed
/// at its job — stopping it early converts a slow useless turn into a fast
/// partial answer.
pub const CONTEXT_HARD: i64 = 48_000;

pub struct Explore {
    provider: Arc<dyn Provider>,
    /// The tools the sub-agent may use. Built once by [`Explore::new`] and
    /// shared by every exploration, the same way the main session shares one.
    registry: Arc<Registry>,
    root: PathBuf,
    sampling: Sampling,
}

impl Explore {
    /// Build the tool around a provider and a workspace root.
    ///
    /// `extra` is for tools the app has that this crate cannot construct —
    /// `web_search` in practice, which needs a key. They are appended to the
    /// read-only core; passing a writing tool here would defeat the point, which
    /// is why the app passes exactly one thing and this takes no registry.
    pub fn new(
        provider: Arc<dyn Provider>,
        root: impl Into<PathBuf>,
        extra: Vec<Box<dyn Tool>>,
    ) -> Self {
        let mut registry = Registry::new()
            .with(smithy_tools::tools::read::Read)
            .with(smithy_tools::tools::ls::Ls)
            .with(smithy_tools::tools::glob::Glob)
            .with(smithy_tools::tools::grep::Grep)
            .with(smithy_tools::tools::web_fetch::WebFetch::new());
        for tool in extra {
            registry.push(tool);
        }

        Self {
            provider,
            registry: Arc::new(registry),
            root: root.into(),
            // Lower temperature than the main session's 0.6. This job is
            // recall and summary, not generation, and a research agent that
            // gets inventive about what a file contains is worse than useless —
            // its output arrives in the parent's history as fact.
            sampling: Sampling {
                temperature: 0.3,
                ..Sampling::default()
            },
        }
    }

    fn limits(&self) -> Limits {
        Limits {
            max_steps: MAX_STEPS,
            max_seconds: MAX_SECONDS,
            context_warn: CONTEXT_HARD / 2,
            context_hard: CONTEXT_HARD,
            max_parse_retries: 2,
            tool_result_warn_chars: crate::limits::tool_result_warn_for_window(CONTEXT_HARD),
        }
    }

    async fn run_inner(
        &self,
        args: &Value,
        parent_control: Option<&ExecutionControl>,
    ) -> ToolOutput {
        let question = match arg_str(args, "question") {
            Ok(q) => q.trim(),
            Err(e) => return ToolOutput::err(e),
        };
        if question.is_empty() {
            return ToolOutput::err("the question is empty");
        }

        let workspace = match Workspace::open(&self.root) {
            Ok(w) => w,
            Err(e) => return ToolOutput::err(format!("cannot open the workspace: {e}")),
        };
        let ctx = Arc::new(ToolCtx::new(workspace));
        let mut config = SessionConfig::new(system_prompt(&self.root));
        config.limits = self.limits();
        config.sampling = self.sampling.clone();
        let mut session = Session::new(
            self.provider.clone(),
            self.registry.clone(),
            ctx,
            config,
        );
        let task = match arg_str_opt(args, "context").map(str::trim) {
            Some(c) if !c.is_empty() => {
                format!("{question}\n\nWhat the caller already knows:\n{c}")
            }
            _ => question.to_string(),
        };

        let outcome = match parent_control {
            Some(parent) => {
                let control = parent.bounded_by(std::time::Duration::from_secs(MAX_SECONDS));
                session.run_turn_controlled(&task, None, control).await
            }
            None => session.run_turn(&task, None).await,
        };
        match outcome {
            Ok(Outcome::Answer(answer)) if answer.trim().is_empty() => ToolOutput::err(
                "the sub-agent returned nothing. Investigate directly with `grep` and `read`."
                    .to_string(),
            ),
            Ok(Outcome::Answer(answer)) => ToolOutput::ok(answer),
            Ok(Outcome::Stopped(reason)) => ToolOutput::ok(format!(
                "[Partial: the sub-agent stopped before answering — {reason}. It produced no \
                 findings. Do not call `explore` again for this; narrow the question or \
                 investigate directly with `grep` and `read`.]"
            )),
            Err(e) => ToolOutput::err(format!("the sub-agent failed: {e}")),
        }
    }
}

#[async_trait]
impl Tool for Explore {
    fn name(&self) -> &'static str {
        "explore"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new(
            "explore",
            "Delegate a self-contained research question to a read-only sub-agent, which \
             searches on its own and returns a short written answer with file:line citations. \
             Its intermediate reads never enter this conversation, so use it when finding the \
             answer would take many searches but the answer itself is short.\n\n\
             Worth it: \"where is retry logic implemented and what backoff does it use\", \"which \
             call sites construct a Workspace\", \"how does crate X's API differ between 0.4 and \
             0.5\".\n\n\
             Not worth it: anything you can answer with one or two `grep`/`read` calls — just do \
             that, it is faster and costs less. Do not use it to write or change code; it \
             cannot. Do not use it for a question about a file you have already read.\n\n\
             Ask one specific question per call. It has a budget of about a dozen tool calls, so \
             a vague question comes back vague. Two of these per turn is a lot; if two have not \
             answered it, investigate directly instead.",
            vec![
                ToolParameter::string(
                    "question",
                    "One specific question, phrased as what you need to know rather than what to \
                     search for.",
                    true,
                ),
                ToolParameter::string(
                    "context",
                    "Optional: what you already know, so it does not rediscover it.",
                    false,
                ),
            ],
        )
    }

    async fn run(&self, args: &Value, _ctx: &ToolCtx) -> ToolOutput {
        self.run_inner(args, None).await
    }

    async fn run_controlled(
        &self,
        args: &Value,
        _ctx: &ToolCtx,
        control: &ExecutionControl,
    ) -> ToolOutput {
        self.run_inner(args, Some(control)).await
    }
}

/// The sub-agent's whole brief.
///
/// Deliberately not [`crate::session::default_system_prompt`]: that one describes
/// an agent that edits code, and most of it is about a job this one cannot do.
/// What it says instead is shaped by the same finding that shaped the tool
/// description — vague delegation produces overlapping, unfocused work, so the
/// objective, the output format and the boundaries are all stated outright.
fn system_prompt(root: &std::path::Path) -> String {
    format!(
        "You are a research sub-agent inside the Smithy IDE. The workspace root is `{}`.\n\n\
         You are answering ONE question for another agent, which cannot see anything you do — \
         only your final message. Everything needed must be in that message.\n\n\
         You are read-only. You have `read`, `ls`, `glob`, `grep`, and `web_fetch` (and \
         `web_search` when configured). You cannot write, edit, or run commands, and must not \
         claim to have done so.\n\n\
         How to work:\n\
         - Search before reading. `grep` to find candidates, then `read` only what looks \
           relevant. Read a slice of a large file, not the whole thing.\n\
         - Issue independent searches in the same step rather than one at a time.\n\
         - You have a budget of about {MAX_STEPS} tool calls. Spend fewer on an easy question. \
           When the budget runs low, answer with what you have.\n\n\
         Your final message must be:\n\
         - A direct answer to the question, first, in a few sentences.\n\
         - Then the evidence, as `path:line` citations. Cite what you actually opened; never \
           invent a line number.\n\
         - Then, if anything is unresolved, one line saying exactly what you could not \
           establish.\n\n\
         Do not pad. Do not restate the question. Do not describe your search process — the \
         caller wants the finding, not the journey. If the answer is \"there is no such thing in \
         this codebase\", say that plainly; it is a useful answer and a confident wrong guess is \
         not.",
        root.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::test_support::{answer, ScriptedProvider};

    fn explore_in(root: &std::path::Path) -> Explore {
        Explore::new(Arc::new(ScriptedProvider::new(vec![])), root, Vec::new())
    }

    /// The invariant that makes this safe to hand a model: a research agent
    /// that could write would bypass the review gate, which is installed on the
    /// *parent* registry and not on this one.
    #[test]
    fn the_sub_agent_cannot_write_edit_or_run_commands() {
        let tmp = tempfile::tempdir().unwrap();
        let names = explore_in(tmp.path()).registry.names();
        for forbidden in ["write", "edit", "bash"] {
            assert!(
                !names.contains(&forbidden),
                "`{forbidden}` must not be reachable from `explore`, got {names:?}"
            );
        }
    }

    /// Without this, a hard question recurses.
    #[test]
    fn the_sub_agent_cannot_call_explore() {
        let tmp = tempfile::tempdir().unwrap();
        let names = explore_in(tmp.path()).registry.names();
        assert!(!names.contains(&"explore"), "{names:?}");
    }

    #[test]
    fn the_sub_agent_can_search_read_and_fetch() {
        let tmp = tempfile::tempdir().unwrap();
        let names = explore_in(tmp.path()).registry.names();
        for expected in ["read", "ls", "glob", "grep", "web_fetch"] {
            assert!(names.contains(&expected), "{names:?}");
        }
    }

    /// The app appends `web_search` here when a key exists; anything it appends
    /// must actually arrive.
    #[test]
    fn extra_tools_are_added_to_the_restricted_set() {
        let tmp = tempfile::tempdir().unwrap();
        let explore = Explore::new(
            Arc::new(ScriptedProvider::new(vec![])),
            tmp.path(),
            vec![Box::new(smithy_tools::tools::web_search::WebSearch::new(
                "test-key",
            ))],
        );
        assert!(explore.registry.names().contains(&"web_search"));
    }

    /// A sub-agent budget at the main session's ceiling would defeat the
    /// purpose — the point is that it stops early and reports partially.
    #[test]
    fn the_budget_is_much_tighter_than_the_main_sessions() {
        let tmp = tempfile::tempdir().unwrap();
        let limits = explore_in(tmp.path()).limits();
        let default = Limits::default();
        assert!(limits.max_steps < default.max_steps);
        assert!(limits.max_seconds < default.max_seconds);
        assert!(limits.context_hard < default.context_hard);
        assert_eq!(limits.max_steps, MAX_STEPS);
    }

    /// The number in the prompt and the number the budget enforces have to be
    /// the same one, or the sub-agent is told a budget it does not have.
    #[test]
    fn the_prompt_states_the_budget_it_is_actually_given() {
        let prompt = system_prompt(std::path::Path::new("/w"));
        assert!(prompt.contains(&MAX_STEPS.to_string()), "{prompt}");
    }

    #[test]
    fn the_prompt_names_the_workspace_root_and_the_read_only_rule() {
        let prompt = system_prompt(std::path::Path::new("/some/root"));
        assert!(prompt.contains("/some/root"), "{prompt}");
        assert!(prompt.contains("read-only"), "{prompt}");
        assert!(prompt.contains("path:line"), "{prompt}");
    }

    /// The description carries the effort budget and the "do not use it for
    /// this" cases, which is the part that stops it being called for a
    /// one-grep question.
    #[test]
    fn the_description_says_when_not_to_use_it() {
        let description = explore_in(std::path::Path::new("/w"))
            .definition()
            .description;
        assert!(description.contains("Not worth it"), "{description}");
        assert!(description.contains("dozen"), "{description}");
    }

    #[tokio::test]
    async fn an_empty_question_is_refused_before_any_model_call() {
        let tmp = tempfile::tempdir().unwrap();
        let out = explore_in(tmp.path())
            .run(
                &serde_json::json!({"question": "   "}),
                &ToolCtx::new(Workspace::open(tmp.path()).unwrap()),
            )
            .await;
        assert!(out.is_error);
    }

    #[tokio::test]
    async fn an_answer_comes_back_as_the_tool_result() {
        let tmp = tempfile::tempdir().unwrap();
        let explore = Explore::new(
            Arc::new(ScriptedProvider::new(vec![answer(
                "Retries live in src/retry.rs:42, with exponential backoff.",
            )])),
            tmp.path(),
            Vec::new(),
        );
        let out = explore
            .run(
                &serde_json::json!({"question": "where are retries?"}),
                &ToolCtx::new(Workspace::open(tmp.path()).unwrap()),
            )
            .await;
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("src/retry.rs:42"), "{}", out.content);
    }

    /// The caller has to be able to tell a complete answer from a truncated
    /// one, or it will act on half a finding as though it were whole.
    #[tokio::test]
    async fn a_budget_stop_is_labelled_partial() {
        let tmp = tempfile::tempdir().unwrap();
        // An empty completion repeated past `max_parse_retries` ends the turn
        // as `Stopped` rather than as an answer.
        let script = vec![
            crate::provider::test_support::empty(),
            crate::provider::test_support::empty(),
            crate::provider::test_support::empty(),
        ];
        let explore = Explore::new(
            Arc::new(ScriptedProvider::new(script)),
            tmp.path(),
            Vec::new(),
        );
        let out = explore
            .run(
                &serde_json::json!({"question": "anything"}),
                &ToolCtx::new(Workspace::open(tmp.path()).unwrap()),
            )
            .await;
        assert!(out.content.contains("Partial"), "{}", out.content);
    }

    /// Prior knowledge is passed through so the sub-agent does not spend its
    /// budget rediscovering what the caller already said.
    #[tokio::test]
    async fn supplied_context_reaches_the_sub_agent() {
        let tmp = tempfile::tempdir().unwrap();
        let explore = Explore::new(
            Arc::new(ScriptedProvider::new(vec![answer("ok")])),
            tmp.path(),
            Vec::new(),
        );
        let out = explore
            .run(
                &serde_json::json!({
                    "question": "where next?",
                    "context": "already read src/main.rs"
                }),
                &ToolCtx::new(Workspace::open(tmp.path()).unwrap()),
            )
            .await;
        assert!(!out.is_error, "{}", out.content);
    }
}
