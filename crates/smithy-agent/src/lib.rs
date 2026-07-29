//! smithy-agent — the agent loop.
//!
//! This is coda's loop, which was by a wide margin the best of the four in the
//! source projects and the only one whose design was driven by measurement
//! rather than assumption. What it brings, and why each part is load-bearing:
//!
//! - **Append-only history.** Prefix caching on a local endpoint is a strict
//!   prefix match: a warm identical prefix is dramatically faster to prefill,
//!   and changing one early token reverts to the full cold cost. So history is
//!   never mutated, reordered, or re-rendered, and the system prompt and tool
//!   schemas are byte-stable for the life of a session.
//! - **[`Budget`].** Step, wall-clock, and context ceilings, with the token
//!   count read from the API's own `usage.prompt_tokens` rather than a local
//!   tokenizer. Cheapest possible protection against a runaway loop.
//! - **Empty or truncated answers are failures, not finishes.** At high context
//!   the model can reason correctly but loop inside its reasoning channel until
//!   it hits the token limit, emitting empty content. Treating that as a clean
//!   "done" produces a silent no-op turn; [`Session`] retries with a targeted
//!   nudge instead.
//! - **[`parse`].** Structured `tool_calls` first, with an XML fallback for
//!   Hermes and Qwen shapes and a repair for unclosed `<think>` blocks.
//!
//! ## What changed in the port
//!
//! **Provider-agnostic.** coda spoke only to LM Studio through a hand-rolled
//! blocking client. The loop here runs against any [`Provider`], so the same
//! machinery drives a local endpoint or a hosted one.
//!
//! **Tool results are correlated by id.** coda appended tool results as
//! positional `user`-role `<tool_response>` messages with no link back to the
//! call they answered, and its post-mortem flagged the fragility: *"I observed
//! one success and moved on; the positional coupling is fragile and untested at
//! scale."* Results here carry `tool_call_id`, so parallel calls cannot be
//! mismatched. See [`message::Message::tool_result`].
//!
//! **Sessions persist.** coda forbade cross-session history as a product
//! constraint. An IDE needs restore, so [`persist`] adds it — but in the one
//! shape that does not throw away the property the constraint was protecting:
//! history is replayed byte-identically rather than re-rendered, so a resumed
//! session still hits a warm prefix.

pub mod limits;
pub mod message;
pub mod parse;
pub mod persist;
pub mod provider;
pub mod providers;
pub mod session;

pub use limits::{Budget, Limits, Stop};
pub use message::{History, Message, Role};
pub use parse::{parse, Action};
pub use persist::{transcript, SessionStore, TranscriptEntry};
pub use provider::{Completion, CompletionRequest, Delta, Provider, ProviderError, Sampling};
pub use providers::{create_provider_from_env, LmStudio, ModelInfo, OpenRouter};
pub use session::{Outcome, Session, SessionConfig, Stopper, TurnEvent, CANCELLED};
